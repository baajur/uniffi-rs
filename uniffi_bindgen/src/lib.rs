/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! # Uniffi: easily build cross-platform software components in Rust
//!
//! This is a highly-experimental crate for building cross-language software components
//! in Rust, based on things we've learned and patterns we've developed in the
//! [mozilla/application-services](https://github.com/mozilla/application-services) project.
//!
//! The idea is to let you write your code once, in Rust, and then re-use it from many
//! other programming languages via Rust's C-compatible FFI layer and some automagically
//! generated binding code. If you think of it as a kind of [wasm-bindgen](https://github.com/rustwasm/wasm-bindgen)
//! wannabe, with a clunkier developer experience but support for more target languages,
//! you'll be pretty close to the mark.
//!
//! Currently supported target languages include Kotlin, Swift and Python.
//!
//! ## Usage
//
//! To build a cross-language component using `uniffi`, follow these steps.
//!
//! ### 1) Specify your Component Interface
//!
//! Start by thinking about the interface you want to expose for use
//! from other languages. Use the Interface Definition Language to specify your interface
//! in a `.udl` file, where it can be processed by the tools from this crate.
//! For example you might define an interface like this:
//!
//! ```text
//! namespace example {
//!   u32 foo(u32 bar);
//! }
//!
//! dictionary MyData {
//!   u32 num_foos;
//!   bool has_a_bar;
//! }
//! ```
//!
//! ### 2) Implement the Component Interface as a Rust crate
//!
//! With the interface, defined, provide a corresponding implementation of that interface
//! as a standard-looking Rust crate, using functions and structs and so-on. For example
//! an implementation of the above Component Interface might look like this:
//!
//! ```text
//! fn foo(bar: u32) -> u32 {
//!     // TODO: a better example!
//!     bar + 42
//! }
//!
//! struct MyData {
//!   num_foos: u32,
//!   has_a_bar: bool
//! }
//! ```
//!
//! ### 3) Generate and include component scaffolding from the UDL file
//!
//! First you will need to install `uniffi-bindgen` on your system using `cargo install uniffi_bindgen`.
//! Then add to your crate `uniffi_build` under `[build-dependencies]`.
//! Finally, add a `build.rs` script to your crate and have it call [uniffi_build::generate_scaffolding](uniffi_build::generate_scaffolding)
//! to process your `.udl` file. This will generate some Rust code to be included in the top-level source
//! code of your crate. If your UDL file is named `example.udl`, then your build script would call:
//!
//! ```text
//! uniffi_build::generate_scaffolding("./src/example.udl")
//! ```
//!
//! This would output a rust file named `example.uniffi.rs`, ready to be
//! included into the code of your rust crate like this:
//!
//! ```text
//! include!(concat!(env!("OUT_DIR"), "/example.uniffi.rs"));
//! ```
//!
//! ### 4) Generate foreign language bindings for the library
//!
//! The `uniffi-bindgen` utility provides a command-line tool that can produce code to
//! consume the Rust library in any of several supported languages.
//! It is done by calling (in kotlin for example):
//!
//! ```text
//! uniffi-bindgen --language kotlin ./src/example.udl
//! ```
//!
//! This will produce a file `example.kt` in the same directory as the .udl file, containing kotlin bindings
//! to load and use the compiled rust code via its C-compatible FFI.
//!

#![warn(rust_2018_idioms)]
#![allow(unknown_lints)]

const BINDGEN_VERSION: &str = env!("CARGO_PKG_VERSION");

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::convert::TryInto;
use std::io::prelude::*;
use std::{
    collections::HashMap,
    env,
    fs::File,
    path::{Path, PathBuf},
    process::Command,
};

pub mod bindings;
pub mod interface;
pub mod scaffolding;

use bindings::TargetLanguage;
use interface::ComponentInterface;
use scaffolding::RustScaffolding;

// Generate the infrastructural Rust code for implementing the UDL interface,
// such as the `extern "C"` function definitions and record data types.
pub fn generate_component_scaffolding<P: AsRef<Path>>(
    udl_file: P,
    config_file_override: Option<P>,
    out_dir_override: Option<P>,
    manifest_path_override: Option<P>,
    format_code: bool,
) -> Result<()> {
    let manifest_path_override = manifest_path_override.as_ref().map(|p| p.as_ref());
    let config_file_override = config_file_override.as_ref().map(|p| p.as_ref());
    let out_dir_override = out_dir_override.as_ref().map(|p| p.as_ref());
    let udl_file = udl_file.as_ref();
    let component = parse_udl(&udl_file)?;
    let _config = get_config(&component, udl_file, config_file_override);
    ensure_versions_compatibility(&udl_file, manifest_path_override)?;
    let mut filename = Path::new(&udl_file)
        .file_stem()
        .ok_or_else(|| anyhow!("not a file"))?
        .to_os_string();
    filename.push(".uniffi.rs");
    let mut out_dir = get_out_dir(&udl_file, out_dir_override)?;
    out_dir.push(filename);
    let mut f =
        File::create(&out_dir).map_err(|e| anyhow!("Failed to create output file: {:?}", e))?;
    write!(f, "{}", RustScaffolding::new(&component))
        .map_err(|e| anyhow!("Failed to write output file: {:?}", e))?;
    if format_code {
        Command::new("rustfmt").arg(&out_dir).status()?;
    }
    Ok(())
}

// If the crate for which we are generating bindings for depends on
// a `uniffi` runtime version that doesn't agree with our own version,
// the developer of that said crate will be in a world of pain.
fn ensure_versions_compatibility(
    udl_file: &Path,
    manifest_path_override: Option<&Path>,
) -> Result<()> {
    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    // If --manifest-path is not provided, we run cargo `metadata` in the .udl dir.
    match manifest_path_override {
        Some(p) => {
            metadata_cmd.manifest_path(p);
        }
        None => {
            metadata_cmd.current_dir(guess_crate_root(udl_file)?);
        }
    };
    let metadata = metadata_cmd
        .exec()
        .map_err(|e| anyhow!("Failed to run cargo metadata: {:?}", e))?;
    let uniffi_runtime_deps: Vec<cargo_metadata::Package> = metadata
        .packages
        .into_iter()
        .filter(|p| p.name == "uniffi")
        .collect();
    if uniffi_runtime_deps.is_empty() {
        bail!("It looks like the crate doesn't depend on the `uniffi` runtime. Please add `uniffi` as a dependency.");
    }
    if uniffi_runtime_deps.len() > 1 {
        bail!("It looks like the workspace depends on multiple versions of `uniffi`. Please rectify the problem and try again.");
    }
    // XXX: Because we're still < 1.0.0, we compare the entire version string.
    // Once we ship v1, we should compare only the MAJOR component.
    let uniffi_runtime_version = uniffi_runtime_deps[0].version.to_string();
    if uniffi_runtime_version != BINDGEN_VERSION {
        bail!("The `uniffi` dependency version ({}) is different than `uniffi-bindgen` own version ({}). Please rectify the problem and try again.", uniffi_runtime_version, BINDGEN_VERSION);
    }
    Ok(())
}

// Generate the bindings in the target languages that call the scaffolding
// Rust code.
pub fn generate_bindings<P: AsRef<Path>>(
    udl_file: P,
    config_file_override: Option<P>,
    target_languages: Vec<&str>,
    out_dir_override: Option<P>,
    try_format_code: bool,
) -> Result<()> {
    let out_dir_override = out_dir_override.as_ref().map(|p| p.as_ref());
    let config_file_override = config_file_override.as_ref().map(|p| p.as_ref());
    let udl_file = udl_file.as_ref();

    let component = parse_udl(&udl_file)?;
    let config = get_config(&component, udl_file, config_file_override)?;
    let out_dir = get_out_dir(&udl_file, out_dir_override)?;
    for language in target_languages {
        bindings::write_bindings(
            &config.bindings,
            &component,
            &out_dir,
            language.try_into()?,
            try_format_code,
            false,
        )?;
    }
    Ok(())
}

// Run tests against the foreign language bindings (generated and compiled at the same time).
// Note that the cdylib we're testing against must be built already.
pub fn run_tests<P: AsRef<Path>>(
    cdylib_dir: P,
    udl_file: P,
    test_scripts: Vec<&str>,
    config_file_override: Option<P>,
) -> Result<()> {
    let cdylib_dir = cdylib_dir.as_ref();
    let udl_file = udl_file.as_ref();
    let config_file_override = config_file_override.as_ref().map(|p| p.as_ref());

    let component = parse_udl(&udl_file)?;
    let config = get_config(&component, udl_file, config_file_override)?;

    // Group the test scripts by language first.
    let mut language_tests: HashMap<TargetLanguage, Vec<String>> = HashMap::new();
    for test_script in test_scripts {
        let lang: TargetLanguage = PathBuf::from(test_script)
            .extension()
            .ok_or_else(|| anyhow!("File has no extension!"))?
            .try_into()?;
        language_tests
            .entry(lang)
            .or_default()
            .push(test_script.to_owned());
    }

    for (lang, test_scripts) in language_tests {
        bindings::write_bindings(&config.bindings, &component, &cdylib_dir, lang, true, true)?;
        bindings::compile_bindings(&config.bindings, &component, &cdylib_dir, lang)?;
        for test_script in test_scripts {
            bindings::run_script(cdylib_dir, &test_script, lang)?;
        }
    }
    Ok(())
}

/// Guess the root directory of the crate from the path of its UDL file.
///
/// For now, we assume that the UDL file is in `./src/something.udl` relative
/// to the crate root. We might consider something more sophisticated in
/// future.
fn guess_crate_root(udl_file: &Path) -> Result<&Path> {
    let path_guess = udl_file
        .parent()
        .ok_or_else(|| anyhow!("UDL file has no parent folder!"))?
        .parent()
        .ok_or_else(|| anyhow!("UDL file has no grand-parent folder!"))?;
    if !path_guess.join("Cargo.toml").is_file() {
        bail!("UDL file does not appear to be inside a crate")
    }
    Ok(path_guess)
}

fn get_config(
    component: &ComponentInterface,
    udl_file: &Path,
    config_file_override: Option<&Path>,
) -> Result<Config> {
    let default_config: Config = component.into();

    let config_file: Option<PathBuf> = match config_file_override {
        Some(cfg) => Some(PathBuf::from(cfg)),
        None => {
            let crate_root = guess_crate_root(udl_file)?.join("uniffi.toml");
            match crate_root.canonicalize() {
                Ok(f) => Some(f),
                Err(_) => None,
            }
        }
    };

    match config_file {
        Some(path) => {
            let contents = slurp_file(&path)
                .with_context(|| format!("Failed to read config file from {:?}", &path))?;
            let loaded_config: Config = toml::de::from_str(&contents)
                .with_context(|| format!("Failed to generate config from file {:?}", &path))?;
            Ok(loaded_config.merge_with(&default_config))
        }
        None => Ok(default_config),
    }
}

fn get_out_dir(udl_file: &Path, out_dir_override: Option<&Path>) -> Result<PathBuf> {
    Ok(match out_dir_override {
        Some(s) => {
            // Create the directory if it doesn't exist yet.
            std::fs::create_dir_all(&s)?;
            s.canonicalize()
                .map_err(|e| anyhow!("Unable to find out-dir: {:?}", e))?
        }
        None => udl_file
            .parent()
            .ok_or_else(|| anyhow!("File has no parent directory"))?
            .to_owned(),
    })
}

fn parse_udl(udl_file: &Path) -> Result<ComponentInterface> {
    let udl =
        slurp_file(udl_file).map_err(|_| anyhow!("Failed to read UDL from {:?}", &udl_file))?;
    udl.parse::<interface::ComponentInterface>()
        .map_err(|e| anyhow!("Failed to parse UDL: {}", e))
}

fn slurp_file(file_name: &Path) -> Result<String> {
    let mut contents = String::new();
    let mut f = File::open(file_name)?;
    f.read_to_string(&mut contents)?;
    Ok(contents)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    bindings: bindings::Config,
}

impl From<&ComponentInterface> for Config {
    fn from(ci: &ComponentInterface) -> Self {
        Config {
            bindings: ci.into(),
        }
    }
}

pub trait MergeWith {
    fn merge_with(&self, other: &Self) -> Self;
}

impl MergeWith for Config {
    fn merge_with(&self, other: &Self) -> Self {
        Config {
            bindings: self.bindings.merge_with(&other.bindings),
        }
    }
}

impl<T: Clone> MergeWith for Option<T> {
    fn merge_with(&self, other: &Self) -> Self {
        match (self, other) {
            (Some(_), _) => self.clone(),
            (None, Some(_)) => other.clone(),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_guessing_of_crate_root_directory_from_udl_file() {
        // When running this test, this will be the ./uniffi_bindgen directory.
        let this_crate_root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());

        let example_crate_root = this_crate_root
            .parent()
            .expect("should have a parent directory")
            .join("./examples/arithmetic");
        assert_eq!(
            guess_crate_root(&example_crate_root.join("./src/arthmetic.udl")).unwrap(),
            example_crate_root
        );

        let not_a_crate_root = &this_crate_root.join("./src/templates");
        assert!(guess_crate_root(&not_a_crate_root.join("./src/example.udl")).is_err());
    }
}

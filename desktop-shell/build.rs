use std::path::PathBuf;

fn main() {
    // Strata design system shared with the other UI crate. In the repository it
    // lives at ../ui-kit; Buildroot copies it to ./ui-kit inside the build dir.
    let kit = ["ui-kit", "../ui-kit"]
        .into_iter()
        .map(PathBuf::from)
        .find(|dir| dir.join("strata.slint").is_file())
        .expect("ui-kit/strata.slint not found next to or above this crate");
    println!("cargo:rerun-if-changed={}", kit.display());

    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_include_paths(vec![kit]);
    slint_build::compile_with_config("ui/shell.slint", config)
        .expect("Failed to compile Slint UI definition");
}

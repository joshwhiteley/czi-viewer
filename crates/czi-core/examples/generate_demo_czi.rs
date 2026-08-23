//! Generate the small, synthetic CZI used for documentation demos.

#[path = "../tests/support/synthetic_czi.rs"]
mod synthetic_czi;

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        return Err(
            "usage: cargo run -p czi-core --example generate_demo_czi -- <output.czi>".into(),
        );
    };
    if path.exists() {
        return Err(format!("refusing to replace existing file: {}", path.display()).into());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, synthetic_czi::demo_czi())?;
    println!("wrote {}", path.display());
    Ok(())
}

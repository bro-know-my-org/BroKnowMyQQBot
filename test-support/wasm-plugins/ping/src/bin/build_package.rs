use std::{env, fs, io::Write as _, path::PathBuf};

use base64::Engine as _;
use wit_component::ComponentEncoder;
use zip::{ZipWriter, write::SimpleFileOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let core_wasm = PathBuf::from(arguments.next().ok_or("missing core Wasm input path")?);
    let manifest = PathBuf::from(arguments.next().ok_or("missing plugin manifest path")?);
    let output = PathBuf::from(
        arguments
            .next()
            .ok_or("missing base64 package output path")?,
    );
    if arguments.next().is_some() {
        return Err("unexpected additional argument".into());
    }

    let component = ComponentEncoder::default()
        .module(&fs::read(core_wasm)?)?
        .validate(true)
        .encode()?;
    let manifest = fs::read(manifest)?;
    let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    archive.start_file("plugin.toml", options)?;
    archive.write_all(&manifest)?;
    archive.start_file("component.wasm", options)?;
    archive.write_all(&component)?;
    let package = archive.finish()?.into_inner();
    let mut encoded = base64::engine::general_purpose::STANDARD.encode(package);
    encoded.push('\n');
    fs::write(output, encoded)?;
    Ok(())
}

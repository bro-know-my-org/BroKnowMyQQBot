use std::{env, fs, path::PathBuf};

use wit_component::ComponentEncoder;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = env::args_os().skip(1);
    let input = PathBuf::from(arguments.next().ok_or("missing core Wasm input path")?);
    let output = PathBuf::from(arguments.next().ok_or("missing component output path")?);
    if arguments.next().is_some() {
        return Err("unexpected additional argument".into());
    }
    let module = fs::read(input)?;
    let component = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()?;
    fs::write(output, component)?;
    Ok(())
}

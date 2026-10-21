use std::{fs, io, path::Path};

/// Copies a Zed setup into Bloo's own folders the first time Bloo runs, so switching keeps
/// settings, keymap, themes, installed extensions and file history.
///
/// Runs before Bloo creates its folders. Does nothing once Bloo's config folder exists,
/// or if Zed was never used. Language servers are not copied; Bloo downloads its own.
pub fn migrate_from_zed() {
    let home = util::paths::home_dir();
    let bloo_config = paths::config_dir();
    let zed_config = home.join(".config").join("zed");
    if bloo_config.exists() || !zed_config.is_dir() {
        return;
    }

    // Logging isn't set up this early in startup, so report problems on stderr.
    if let Err(error) = copy_dir(&zed_config, bloo_config) {
        eprintln!("Bloo: couldn't copy settings from {}: {error}", zed_config.display());
    }

    let zed_data = home.join("Library").join("Application Support").join("Zed");
    for folder in [Path::new("extensions/installed"), Path::new("file_history")] {
        let source = zed_data.join(folder);
        if source.is_dir()
            && let Err(error) = copy_dir(&source, &paths::data_dir().join(folder))
        {
            eprintln!("Bloo: couldn't copy {}: {error}", source.display());
        }
    }
}

/// Recursively copies `source` into `destination`, creating it. Symlinks are skipped.
fn copy_dir(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_nested_folders_and_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("zed");
        fs::create_dir_all(source.join("themes")).unwrap();
        fs::write(source.join("settings.json"), "{\"theme\": \"x\"}").unwrap();
        fs::write(source.join("themes/bloo.json"), "{}").unwrap();

        let destination = temp.path().join("bloo");
        copy_dir(&source, &destination).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("settings.json")).unwrap(),
            "{\"theme\": \"x\"}"
        );
        assert!(destination.join("themes/bloo.json").is_file());
    }
}

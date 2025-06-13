use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

use anyhow::Result;
use bootc_utils::CommandRunExt;
use fn_error_context::context;
use openat_ext::OpenatDirExt;
use rustix::fd::BorrowedFd;
use serde::Deserialize;

use crate::blockdev;
use crate::efi::Efi;
use std::path::Path;

#[derive(Deserialize, Debug)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub(crate) struct Filesystem {
    pub(crate) source: String,
    pub(crate) fstype: String,
    pub(crate) options: String,
    pub(crate) uuid: Option<String>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct Findmnt {
    pub(crate) filesystems: Vec<Filesystem>,
}

#[context("Inspecting filesystem {path:?}")]
pub(crate) fn inspect_filesystem(root: &openat::Dir, path: &str) -> Result<Filesystem> {
    let rootfd = unsafe { BorrowedFd::borrow_raw(root.as_raw_fd()) };
    // SAFETY: This is unsafe just for the pre_exec, when we port to cap-std we can use cap-std-ext
    let o: Findmnt = unsafe {
        Command::new("findmnt")
            .args(["-J", "-v", "--output=SOURCE,FSTYPE,OPTIONS,UUID", path])
            .pre_exec(move || rustix::process::fchdir(rootfd).map_err(Into::into))
            .run_and_parse_json()?
    };
    o.filesystems
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("findmnt returned no data"))
}

#[context("Copying {file_path} from {src_root} to {dest_root}")]
pub(crate) fn copy_files(src_root: &str, dest_root: &str, file_path: &str) -> Result<()> {
    let src_dir = openat::Dir::open(src_root)?;
    let file_path = file_path.strip_prefix("/").unwrap_or(file_path);
    let dest_dir = if file_path.starts_with("boot/efi") {
        let efi = Efi::default();
        match blockdev::get_single_device("/") {
            Ok(device) => {
                let esp_device = blockdev::get_esp_partition(&device)?;
                let esp_path = efi.ensure_mounted_esp(
                    Path::new(dest_root),
                    Path::new(&esp_device.unwrap_or_default()),
                )?;
                openat::Dir::open(&esp_path)?
            }
            Err(e) => anyhow::bail!("Unable to find device: {}", e),
        }
    } else {
        openat::Dir::open(dest_root)?
    };

    let src_meta = src_dir.metadata(file_path)?;
    match src_meta.simple_type() {
        openat::SimpleType::File => {
            let parent = Path::new(file_path).parent().unwrap_or(Path::new("."));
            if !parent.as_os_str().is_empty() {
                dest_dir.ensure_dir_all(parent, 0o755)?;
            }
            src_dir.copy_file_at(file_path, &dest_dir, file_path)?;
            log::info!("Copied file: {} to destination", file_path);
        }
        openat::SimpleType::Dir => {
            anyhow::bail!("Unsupported copying of Directory {}", file_path)
        }
        openat::SimpleType::Symlink => {
            anyhow::bail!("Unsupported symbolic link {}", file_path)
        }
        openat::SimpleType::Other => {
            anyhow::bail!("Unsupported non-file/directory {}", file_path)
        }
    }

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use anyhow::Result;
    use openat_ext::OpenatDirExt;
    use std::fs as std_fs;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn test_copy_single_file_basic() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_root_dir = openat::Dir::open(tmp.path())?;

        let src_root_name = "src_root";
        let dest_root_name = "dest_root";

        tmp_root_dir.create_dir(src_root_name, 0o755)?;
        tmp_root_dir.create_dir(dest_root_name, 0o755)?;

        let src_dir = tmp_root_dir.sub_dir(src_root_name)?;

        let file_to_copy_rel = "file.txt";
        let content = "This is a test file.";

        // Create source file using
        src_dir.write_file_contents(file_to_copy_rel, 0o644, content.as_bytes())?;

        let src_root_abs_path_str = tmp.path().join(src_root_name).to_str().unwrap().to_string();
        let dest_root_abs_path_str = tmp
            .path()
            .join(dest_root_name)
            .to_str()
            .unwrap()
            .to_string();

        copy_files(
            &src_root_abs_path_str,
            &dest_root_abs_path_str,
            file_to_copy_rel,
        )?;

        let dest_file_abs_path = tmp.path().join(dest_root_name).join(file_to_copy_rel);
        assert!(dest_file_abs_path.exists(), "Destination file should exist");
        assert_eq!(
            std_fs::read_to_string(&dest_file_abs_path)?,
            content,
            "File content should match"
        );

        Ok(())
    }

    #[test]
    fn test_copy_file_in_subdirectory() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_root_dir = openat::Dir::open(tmp.path())?;

        let src_root_name = "src";
        let dest_root_name = "dest";

        tmp_root_dir.create_dir(src_root_name, 0o755)?;
        tmp_root_dir.create_dir(dest_root_name, 0o755)?;

        let src_dir_oat = tmp_root_dir.sub_dir(src_root_name)?;

        let file_to_copy_rel = "subdir/another_file.txt";
        let content = "Content in a subdirectory.";

        // Create subdirectory and file in source
        src_dir_oat.ensure_dir_all("subdir", 0o755)?;
        let mut f = src_dir_oat.write_file("subdir/another_file.txt", 0o644)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;

        let src_root_abs_path_str = tmp.path().join(src_root_name).to_str().unwrap().to_string();
        let dest_root_abs_path_str = tmp
            .path()
            .join(dest_root_name)
            .to_str()
            .unwrap()
            .to_string();

        copy_files(
            &src_root_abs_path_str,
            &dest_root_abs_path_str,
            file_to_copy_rel,
        )?;

        let dest_file_abs_path = tmp.path().join(dest_root_name).join(file_to_copy_rel);
        assert!(
            dest_file_abs_path.exists(),
            "Destination file in subdirectory should exist"
        );
        assert_eq!(
            std_fs::read_to_string(&dest_file_abs_path)?,
            content,
            "File content should match"
        );
        assert!(
            dest_file_abs_path.parent().unwrap().is_dir(),
            "Destination subdirectory should be a directory"
        );

        Ok(())
    }

    #[test]
    fn test_copy_file_with_leading_slash_in_filepath_arg() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_root_dir = openat::Dir::open(tmp.path())?;

        let src_root_name = "src";
        let dest_root_name = "dest";

        tmp_root_dir.create_dir(src_root_name, 0o755)?;
        tmp_root_dir.create_dir(dest_root_name, 0o755)?;

        let src_dir_oat = tmp_root_dir.sub_dir(src_root_name)?;

        let file_rel_actual = "root_file.txt";
        let file_arg_with_slash = "/root_file.txt";
        let content = "Leading slash test.";

        src_dir_oat.write_file_contents(file_rel_actual, 0o644, content.as_bytes())?;

        let src_root_abs_path_str = tmp.path().join(src_root_name).to_str().unwrap().to_string();
        let dest_root_abs_path_str = tmp
            .path()
            .join(dest_root_name)
            .to_str()
            .unwrap()
            .to_string();

        copy_files(
            &src_root_abs_path_str,
            &dest_root_abs_path_str,
            file_arg_with_slash,
        )?;

        // The destination path should be based on the path *after* stripping the leading slash
        let dest_file_abs_path = tmp.path().join(dest_root_name).join(file_rel_actual);
        assert!(
            dest_file_abs_path.exists(),
            "Destination file should exist despite leading slash in arg"
        );
        assert_eq!(
            std_fs::read_to_string(&dest_file_abs_path)?,
            content,
            "File content should match"
        );

        Ok(())
    }

    #[test]
    fn test_copy_fails_for_directory() -> Result<()> {
        let tmp = tempdir()?;
        let tmp_root_dir = openat::Dir::open(tmp.path())?;

        let src_root_name = "src";
        let dest_root_name = "dest";

        tmp_root_dir.create_dir(src_root_name, 0o755)?;
        tmp_root_dir.create_dir(dest_root_name, 0o755)?;

        let src_dir_oat = tmp_root_dir.sub_dir(src_root_name)?;

        let dir_to_copy_rel = "a_directory";
        src_dir_oat.create_dir(dir_to_copy_rel, 0o755)?; // Create the directory in the source

        let src_root_abs_path_str = tmp.path().join(src_root_name).to_str().unwrap().to_string();
        let dest_root_abs_path_str = tmp
            .path()
            .join(dest_root_name)
            .to_str()
            .unwrap()
            .to_string();

        let result = copy_files(
            &src_root_abs_path_str,
            &dest_root_abs_path_str,
            dir_to_copy_rel,
        );

        assert!(result.is_err(), "Copying a directory should fail.");
        if let Err(e) = result {
            let mut found_unsupported_message = false;
            for cause in e.chain() {
                // Iterate through the error chain
                if cause
                    .to_string()
                    .contains("Unsupported copying of Directory")
                {
                    found_unsupported_message = true;
                    break;
                }
            }
            assert!(
                found_unsupported_message,
                "The error chain should contain 'Unsupported copying of Directory'. Full error: {:#?}",
                e
            );
        } else {
            panic!("Expected an error when attempting to copy a directory, but got Ok.");
        }
        Ok(())
    }
}

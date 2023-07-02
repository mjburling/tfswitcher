mod ffi;

use clap::Parser;
use dialoguer::{theme::ColorfulTheme, Select};
use regex::Regex;
use reqwest::blocking::Response;
use semver::{Version, VersionReq};
use std::{
    env::{self, consts},
    error::Error,
    fs::{self, File},
    io::{self, Cursor},
    os::unix::prelude::PermissionsExt,
    path::PathBuf,
    str::FromStr,
};
use zip::ZipArchive;

const ARCHIVE_URL: &str = "https://releases.hashicorp.com/terraform";
const DEFAULT_LOCATION: &str = ".local/bin";
const PROGRAM_NAME: &str = "terraform";

#[derive(Parser, Debug)]
struct Args {
    /// Include pre-release versions
    #[arg(short, long = "list-all", default_value_t = false)]
    list_all: bool,

    #[arg(short = 'i', long = "install", env = "TF_VERSION")]
    version: Option<String>,
}

fn find_program_path(program_name: &str) -> Option<PathBuf> {
    if let Ok(path_var) = env::var("PATH") {
        let separator = if cfg!(windows) { ';' } else { ':' };

        for path in path_var.split(separator) {
            let program_path = PathBuf::from(path).join(program_name);
            if program_path.exists() {
                return Some(program_path);
            }
        }
    }

    None
}

fn get_http(url: &str) -> Result<Response, Box<dyn Error>> {
    let response = reqwest::blocking::get(url)?;
    match response.error_for_status_ref() {
        Ok(_) => Ok(response),
        Err(e) => Err(Box::new(e)),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let Some(program_path) = find_terraform_program_path() else {
        panic!("could not find path to install terraform");
    };

    let version = get_version_to_install(args)?;

    install_version(program_path, &version)?;

    Ok(())
}

fn find_terraform_program_path() -> Option<PathBuf> {
    if let Some(path) = find_program_path(PROGRAM_NAME) {
        return Some(path);
    }

    match home::home_dir() {
        Some(mut path) => {
            path.push(format!("{DEFAULT_LOCATION}/{PROGRAM_NAME}"));
            println!("could not locate {PROGRAM_NAME}, installing to {path:?}\nmake sure to include the directory into your $PATH");
            Some(path)
        }
        None => None,
    }
}

fn get_version_to_install(args: Args) -> Result<String, Box<dyn Error>> {
    if let Some(version) = args.version {
        return Ok(version);
    }

    let versions = get_terraform_versions(args, ARCHIVE_URL)?;

    if let Some(version_from_module) = get_version_from_module(&versions)? {
        return Ok(version_from_module);
    }

    get_version_from_user_prompt(&versions)
}

fn get_terraform_versions(args: Args, url: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let response = get_http(url)?;
    let contents = response.text()?;

    let versions = capture_terraform_versions(args, &contents);

    Ok(versions)
}

fn capture_terraform_versions(args: Args, contents: &str) -> Vec<String> {
    let mut versions = vec![];

    let lines: Vec<_> = contents.split('\n').collect();
    // From https://github.com/warrensbox/terraform-switcher/blob/d7dfd1b44605b095937e94b981d24305b858ff8c/lib/list_versions.go#L28-L35
    let re = if args.list_all {
        Regex::new(r#"/(\d+\.\d+\.\d+)(?:-[a-zA-Z0-9-]+)?/?""#).expect("Invalid regex")
    } else {
        Regex::new(r#"/(\d+\.\d+\.\d+)/?""#).expect("Invalid regex")
    };
    let trim_matches: &[_] = &['/', '"'];
    for text in lines {
        if let Some(capture) = re.captures(text) {
            if let Some(mat) = capture.get(0) {
                versions.push(mat.as_str().trim_matches(trim_matches).to_string());
            }
        }
    }

    versions
}

fn get_version_from_module(versions: &[String]) -> Result<Option<String>, Box<dyn Error>> {
    let version_constraint = match ffi::get_version_from_module() {
        Some(constraint) => constraint,
        None => return Ok(None),
    };

    println!("module constraint is {version_constraint}");

    let req = VersionReq::parse(&version_constraint)?;
    for version in versions {
        let v = Version::from_str(version)?;
        if req.matches(&v) {
            return Ok(Some(version.to_owned()));
        }
    }

    Ok(None)
}

fn get_version_from_user_prompt(versions: &[String]) -> Result<String, Box<dyn Error>> {
    let version = prompt_version_to_user(versions)?;

    Ok(version)
}

fn prompt_version_to_user(versions: &[String]) -> Result<String, Box<dyn Error>> {
    println!("select a terraform version to install");
    let selection = Select::with_theme(&ColorfulTheme::default())
        .items(versions)
        .default(0)
        .interact()?;

    Ok(versions[selection].to_owned())
}

fn install_version(program_path: PathBuf, version: &str) -> Result<(), Box<dyn Error>> {
    println!("{PROGRAM_NAME} {version} will be installed to {program_path:?}");

    let os = consts::OS;
    let arch = match consts::ARCH {
        "x86" => "386",
        "x86_64" => "amd64",
        _ => consts::ARCH,
    };

    let archive = get_terraform_version_zip(version, os, arch)?;
    extract_zip_archive(&program_path, archive)
}

fn get_terraform_version_zip(
    version: &str,
    os: &str,
    arch: &str,
) -> Result<ZipArchive<Cursor<Vec<u8>>>, Box<dyn Error>> {
    let zip_name = format!("terraform_{version}_{os}_{arch}.zip");

    if let Some(path) = home::home_dir().as_mut() {
        path.push(format!("{DEFAULT_LOCATION}/{zip_name}"));

        if path.exists() {
            println!("using cached archive at {path:?}");
            let buffer = fs::read(path)?;
            let cursor = Cursor::new(buffer);
            let archive = ZipArchive::new(cursor)?;
            return Ok(archive);
        }
    }

    download_and_save_terraform_version_zip(version, &zip_name)
}

fn download_and_save_terraform_version_zip(
    version: &str,
    zip_name: &str,
) -> Result<ZipArchive<Cursor<Vec<u8>>>, Box<dyn Error>> {
    let url = format!("{ARCHIVE_URL}/{version}/{zip_name}");
    println!("downloading archive from {url}");

    let response = get_http(&url)?;
    let buffer = response.bytes()?.to_vec();

    match home::home_dir() {
        Some(mut path) => {
            path.push(format!("{DEFAULT_LOCATION}/{zip_name}"));
            fs::write(path, &buffer)?;
        }
        None => println!("unable to cache archive"),
    }

    let cursor = Cursor::new(buffer);
    Ok(ZipArchive::new(cursor)?)
}

fn extract_zip_archive(
    program_path: &PathBuf,
    mut archive: ZipArchive<Cursor<Vec<u8>>>,
) -> Result<(), Box<dyn Error>> {
    let mut file = archive.by_index(0)?;
    let file_name = file.name();
    println!("extracting {file_name} to {program_path:?}");

    // Create a new file for the extracted file and set rwxr-xr-x
    let mut outfile = File::create(program_path)?;
    let mut perms = outfile.metadata()?.permissions();
    perms.set_mode(0o755);
    outfile.set_permissions(perms)?;

    // Write the contents of the file to the output file
    io::copy(&mut file, &mut outfile)?;

    println!("extracted archive to {program_path:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, path::Path};
    use tempdir::TempDir;

    const LINES: &str = "<html><head>
        <title>Terraform Versions | HashiCorp Releases</title>

    </head>
    <body>
        <ul>
            <li>
            <a href=\"../\">../</a>
            </li>
            <li>
            <a href=\"/terraform/1.3.0/\">terraform_1.3.0</a>
            </li>
            <li>
            <a href=\"/terraform/1.3.0-rc1/\">terraform_1.3.0-rc1</a>
            </li>
            <li>
            <a href=\"/terraform/1.3.0-beta1/\">terraform_1.3.0-beta1</a>
            </li>
            <li>
            <a href=\"/terraform/1.3.0-alpha20220608/\">terraform_1.3.0-alpha20220608</a>
            </li>
            <li>
            <a href=\"/terraform/1.2.0/\">terraform_1.2.0</a>
            </li>
            <li>
            <a href=\"/terraform/1.2.0-rc1/\">terraform_1.2.0-rc1</a>
            </li>
            <li>
            <a href=\"/terraform/1.2.0-beta1/\">terraform_1.2.0-beta1</a>
            </li>
            <li>
            <a href=\"/terraform/1.2.0-alpha20220413/\">terraform_1.2.0-alpha20220413</a>
            </li>
            <li>
            <a href=\"/terraform/1.2.0-alpha-20220328/\">terraform_1.2.0-alpha-20220328</a>
            </li>
            <li>
            <a href=\"/terraform/1.1.0/\">terraform_1.1.0</a>
            </li>
            <li>
            <a href=\"/terraform/1.1.0-rc1/\">terraform_1.1.0-rc1</a>
            </li>
            <li>
            <a href=\"/terraform/1.1.0-beta1/\">terraform_1.1.0-beta1</a>
            </li>
            <li>
            <a href=\"/terraform/1.1.0-alpha20211029/\">terraform_1.1.0-alpha20211029</a>
            </li>
            <li>
            <a href=\"/terraform/1.0.0/\">terraform_1.0.0</a>
            </li>
            <li>
            <a href=\"/terraform/0.15.0/\">terraform_0.15.0</a>
            </li>
            <li>
            <a href=\"/terraform/0.15.0-rc1/\">terraform_0.15.0-rc1</a>
            </li>
            <li>
            <a href=\"/terraform/0.15.0-beta1/\">terraform_0.15.0-beta1</a>
            </li>
            <li>
            <a href=\"/terraform/0.15.0-alpha20210107/\">terraform_0.15.0-alpha20210107</a>
            </li>
            
        </ul>

</body></html>";

    #[test]
    fn test_capture_terraform_versions() -> Result<(), Box<dyn Error>> {
        let expected_versions = vec!["1.3.0", "1.2.0", "1.1.0", "1.0.0", "0.15.0"];
        let args = Args {
            list_all: false,
            version: None,
        };
        let actual_versions = capture_terraform_versions(args, LINES);

        assert_eq!(expected_versions, actual_versions);

        Ok(())
    }

    #[test]
    fn test_capture_terraform_versions_list_all() -> Result<(), Box<dyn Error>> {
        let expected_versions = vec![
            "1.3.0",
            "1.3.0-rc1",
            "1.3.0-beta1",
            "1.3.0-alpha20220608",
            "1.2.0",
            "1.2.0-rc1",
            "1.2.0-beta1",
            "1.2.0-alpha20220413",
            "1.2.0-alpha-20220328",
            "1.1.0",
            "1.1.0-rc1",
            "1.1.0-beta1",
            "1.1.0-alpha20211029",
            "1.0.0",
            "0.15.0",
            "0.15.0-rc1",
            "0.15.0-beta1",
            "0.15.0-alpha20210107",
        ];
        let args = Args {
            list_all: true,
            version: None,
        };
        let actual_versions = capture_terraform_versions(args, LINES);

        assert_eq!(expected_versions, actual_versions);

        Ok(())
    }

    #[test]
    fn test_get_version_from_module() -> Result<(), Box<dyn Error>> {
        const EXPECTED_VERSION: &str = "1.0.0";
        let versions: Vec<String> = vec![EXPECTED_VERSION.to_string()];

        let tmp_dir = TempDir::new("test_get_version_from_module")?;
        let file_path = tmp_dir.path().join("version.tf");
        let mut file = File::create(&file_path)?;
        file.write_all(b"terraform { required_version = \"1.0.0\" }")?;
        env::set_current_dir(Path::new(&tmp_dir.path()))?;

        let actual_version = get_version_from_module(&versions)?;
        assert!(actual_version.is_some());
        assert_eq!(EXPECTED_VERSION, actual_version.unwrap());

        drop(file);
        tmp_dir.close()?;
        Ok(())
    }
}

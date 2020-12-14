use std::{fmt::Display, path::Path, process::Command, str::FromStr};

use anyhow::anyhow;
use anyhow::{Error, Result};
use clap::arg_enum;
use http_types::StatusCode;
use semver::{Version, VersionReq};
use smol::{fs::File, prelude::*};
use structopt::StructOpt;
use tempfile::tempdir;

mod http;

#[derive(StructOpt)]
struct Arg {
    #[structopt(short, long)]
    version: DotnetVersion,
    #[structopt(short, long, possible_values = &Runtime::variants(), case_insensitive = true)]
    runtime: Runtime,
}

#[derive(Copy, Clone)]
struct DotnetVersion {
    major: u64,
    minor: Option<u64>,
    patch: Option<u64>,
}

impl Display for DotnetVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}", self.major))?;
        
        if let Some(minor) = self.minor {
            f.write_fmt(format_args!(".{}", minor))?;

            if let Some(patch) = self.patch {
                f.write_fmt(format_args!(".{}", patch))?;
            }
        }

        Ok(())
    }
}

impl FromStr for DotnetVersion {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts = s
            .split('.')
            .map(FromStr::from_str)
            .collect::<Result<Vec<u64>, _>>()?;
        let version = match *parts.as_slice() {
            [major] => DotnetVersion {
                major,
                minor: None,
                patch: None,
            },
            [major, minor] => DotnetVersion {
                major,
                minor: Some(minor),
                patch: None,
            },
            [major, minor, patch] => DotnetVersion {
                major,
                minor: Some(minor),
                patch: Some(patch),
            },
            _ => return Err(anyhow!("invalid version number")),
        };

        Ok(version)
    }
}

arg_enum! {
    #[derive(Copy, Clone)]
    enum Runtime {
        Dotnet,
        AspCore,
        WindowsDesktop,
    }
}

const BASE_URL: &str = "https://dotnetcli.blob.core.windows.net/dotnet";
const CDN_URL: &str = "https://dotnetcli.azureedge.net/dotnet";

fn main() -> Result<()> {
    smol::block_on(async {
        let arg: Arg = Arg::from_args();

        if is_installed(arg.runtime, &arg.version).await? {
            return Ok(());
        }

        let version = find_best_version(arg.runtime, arg.version).await?;
        let product_version = find_product_version(arg.runtime, &version).await?;

        let dir = tempdir()?;
        let download_path = dir.path().join("installer.exe");
        let mut file = File::create(&download_path).await?;
        let url = download_url(arg.runtime, version, &product_version);
        let response = http::get(&url).await?;

        if response.status() == StatusCode::Ok {
            smol::io::copy(response, &mut file).await?;
            file.flush().await?;
            std::mem::drop(file);
            Command::new(download_path).arg("/norestart").arg("/quiet").status()?;
            Ok(())
        } else if response.status() == StatusCode::NotFound{
            Err(anyhow!("requested dotnet version does not exist"))
        } else {
            Err(anyhow!("failed to download dotnet version {}", product_version))
        }
    })
}

async fn is_installed(runtime: Runtime, dotnet_version: &DotnetVersion) -> Result<bool> {

    let version_req = VersionReq::parse(&dotnet_version.to_string())?;
    let runtime_path = match runtime {
        Runtime::Dotnet => "shared\\Microsoft.NETCore.App",
        Runtime::AspCore => "shared\\Microsoft.AspNetCore.App",
        Runtime::WindowsDesktop => "shared\\Microsoft.WindowsDesktop.App",
    };

    let root_path = Path::new("C:\\Program Files\\dotnet");
    let mut entries = smol::fs::read_dir(root_path.join(runtime_path)).await?;
    
    while let Some(entry) = entries.try_next().await? {
        let version = Version::parse(&entry.file_name().to_string_lossy())?;
        let file_type = entry.file_type().await?;

        if file_type.is_dir() && version_req.matches(&version) {
            return Ok(true);
        }
    }

    return Ok(false);
}

fn download_url(runtime: Runtime, version: Version, product_version: &str) -> String {
    match runtime {
        Runtime::Dotnet => format!(
            "{}/Runtime/{}/dotnet-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, "x64"
        ),
        Runtime::AspCore => format!(
            "{}/aspnetcore/Runtime/{}/aspnetcore-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, "x64"
        ),
        Runtime::WindowsDesktop => format!(
            "{}/Runtime/{}/windowsdesktop-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, "x64"
        ),
    }
}

async fn find_product_version(runtime: Runtime, version: &Version) -> Result<String> {
    let url = match runtime {
        Runtime::Dotnet | Runtime::WindowsDesktop => {
            format!("{}/Runtime/{}/productVersion.txt", CDN_URL, version)
        }
        Runtime::AspCore => format!("{}/aspnetcore/Runtime{}", BASE_URL, version),
    };

    let mut response = http::get(&url).await?;
    if response.status() == StatusCode::Ok {
        Ok(response
            .body_string()
            .await
            .map_err(Error::msg)?
            .trim()
            .to_string())
    } else {
        Ok(version.to_string())
    }
}

async fn find_best_version(runtime: Runtime, version: DotnetVersion) -> Result<Version> {
    if let DotnetVersion {
        major,
        minor: Some(minor),
        patch: Some(patch),
    } = version
    {
        return Ok(Version::new(major, minor, patch));
    }

    let url = match runtime {
        Runtime::Dotnet | Runtime::WindowsDesktop => format!("{}/Runtime", BASE_URL),
        Runtime::AspCore => format!("{}/aspnetcore/Runtime", BASE_URL),
    };

    let minor = if let Some(minor) = version.minor {
        minor
    } else {
        find_newest_minor(&url, version.major).await?
    };

    let full_url = format!("{}/{}.{}/latest.version", url, version.major, minor);
    let version_text = http::get(&full_url)
        .await?
        .body_string()
        .await
        .map_err(Error::msg)?;

    if let Some(version_text) = version_text.lines().last() {
        Ok(Version::from_str(version_text)?)
    } else {
        Err(anyhow!(
            "version file did not contain expected version text"
        ))
    }
}

async fn find_newest_minor(url: &str, major_version: u64) -> Result<u64> {
    for minor in 0.. {
        let full_url = format!("{}/{}.{}/latest.version", url, major_version, minor);
        let response = http::get(&full_url).await?;
        if StatusCode::NotFound == response.status() {
            if minor > 0 {
                return Ok(minor - 1);
            } else {
                return Err(anyhow!("No available versions found"));
            }
        }
    }

    unreachable!();
}


use std::{path::Path, process::Command, str::FromStr};
use anyhow::{Result, anyhow};
use clap::arg_enum;
use reqwest::StatusCode;
use semver::Version;
use structopt::StructOpt;
use tempfile::{tempdir};
use tokio::io::AsyncWriteExt;

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

#[tokio::main]
async fn main() -> Result<()> {
    let arg: Arg = Arg::from_args();
    
    let version = find_best_version(arg.runtime, arg.version).await?;
    let product_version = find_product_version(arg.runtime, &version).await?;

    if is_installed(arg.runtime, &product_version) {
        return Ok(());
    }

    let dir = tempdir()?;
    let download_path = dir.path().join("installer.exe");
    let mut file = tokio::fs::File::create(&download_path).await?;
    let url = download_url(arg.runtime, version, product_version);
    let mut response = reqwest::get(&url).await?;

    while let Some(chunk) = response.chunk().await? {
        file.write(&chunk).await?;
    }

    file.flush().await?;

    Command::new(download_path).arg("/norestart").status()?;

    Ok(())
}

fn is_installed(runtime: Runtime, product_version: &str) -> bool {
    let runtime_path = match runtime {
        Runtime::Dotnet => "shared\\Microsoft.NETCore.App",
        Runtime::AspCore => "shared\\Microsoft.AspNetCore.App",
        Runtime::WindowsDesktop => "shared\\Microsoft.WindowsDesktop.App",
    };

    let root_path = Path::new("C:\\Program Files\\dotnet");
    root_path.join(runtime_path).join(product_version).is_dir()
}

fn download_url(runtime: Runtime, version: Version, product_version: String) -> String {    
    match runtime {
        Runtime::Dotnet => format!("{}/Runtime/{}/dotnet-runtime-{}-win-{}.exe", BASE_URL, version, product_version, "x64"),
        Runtime::AspCore => format!("{}/aspnetcore/Rumtime/{}/aspnetcore-runtime-{}-win-{}.exe", BASE_URL, version, product_version, "x64"),
        Runtime::WindowsDesktop => format!("{}/Runtime/{}/windowsdesktop-runtime-{}-win-{}.exe", BASE_URL, version, product_version, "x64")
    }
}

async fn find_product_version(runtime: Runtime, version: &Version) -> Result<String> {
    let url = match runtime {
        Runtime::Dotnet | Runtime::WindowsDesktop => format!("{}/Runtime/{}/productVersion.txt", CDN_URL, version),
        Runtime::AspCore => format!("{}/aspnetcore/Runtime{}", BASE_URL, version),
    };

    let response = reqwest::get(&url).await?;

    if response.status() == StatusCode::OK {
        Ok(response.text().await?.trim().to_string())
    } else {
        Ok(version.to_string())
    }
}

async fn find_best_version(runtime: Runtime, version: DotnetVersion) -> Result<Version> {
    if let DotnetVersion{major, minor: Some(minor), patch: Some(patch)} = version {
        return Ok(Version::new(major, minor, patch));
    }
    
    let url = match runtime {
        Runtime::Dotnet | Runtime::WindowsDesktop => format!("{}/Runtime", BASE_URL),
        Runtime::AspCore => format!("{}/aspnetcore/Rumtime", BASE_URL),
    };

    let minor = if let Some(minor) = version.minor {
        minor
    } else {
        find_newest_minor(&url, version.major).await?
    };

    let full_url = format!("{}/{}.{}/latest.version", url, version.major, minor);

    let version_text = reqwest::get(&full_url).await?.text().await?;

    if let Some(version_text) = version_text.lines().last() {
        Ok(Version::from_str(version_text)?)
    } else {
        Err(anyhow!("version file did not contain expected version text"))
    }
}

async fn find_newest_minor(url: &str, major_version: u64) -> Result<u64> {
    let client = reqwest::Client::new();
    for minor in 0.. {
        let full_url = format!("{}/{}.{}/latest.version", url, major_version, minor);
        if StatusCode::NOT_FOUND == client.get(&full_url).send().await?.status() {
            if minor > 0 {
                return Ok(minor - 1);
            } else {
                return Err(anyhow!("No available versions found"))
            }
        }
    }

    unreachable!();
}

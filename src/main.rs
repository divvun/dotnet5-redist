use std::{fmt::Display, path::Path, process::Command, str::FromStr};

use anyhow::{anyhow, bail};
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
    #[structopt(short, long, possible_values = &Architecture::variants(), case_insensitive = true)]
    arch: Architecture,

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

arg_enum! {
    #[derive(Copy, Clone, PartialEq, Eq)]
    enum Architecture {
        X86,
        X64,
    }
}

const BASE_URL: &str = "https://dotnetcli.blob.core.windows.net/dotnet";
const CDN_URL: &str = "https://dotnetcli.azureedge.net/dotnet";

fn main() -> Result<()> {
    smol::block_on(async {
        let arg: Arg = Arg::from_args();

        if arg.arch == Architecture::X64 && !is_64bit_os() {
            bail!("Cannot install 64-bit dotnet on 32-bit windows");
        }

        if !is_vcruntime_installed(arg.arch) {
            let url = match arg.arch {
                Architecture::X86 => "https://download.visualstudio.microsoft.com/download/pr/8ecb9800-52fd-432d-83ee-d6e037e96cc2/50A3E92ADE4C2D8F310A2812D46322459104039B9DEADBD7FDD483B5C697C0C8/VC_redist.x86.exe",
                Architecture::X64 => "https://download.visualstudio.microsoft.com/download/pr/89a3b9df-4a09-492e-8474-8f92c115c51d/B1A32C71A6B7D5978904FB223763263EA5A7EB23B2C44A0D60E90D234AD99178/VC_redist.x64.exe",
            };

            download_install(url).await?;
        }

        if !is_installed(arg.arch, arg.runtime, &arg.version).await? {
            let version = find_best_version(arg.runtime, arg.version).await?;
            let product_version = find_product_version(arg.runtime, &version).await?;

            let url = download_url(arg.arch, arg.runtime, version, &product_version);
            download_install(&url).await?;
        }

        Ok(())
    })
}

async fn download_install(url: &str) -> Result<()> {
    let dir = tempdir()?;
    let download_path = dir.path().join("installer.exe");
    let mut file = File::create(&download_path).await?;
    let response = http::get(&url).await?;

    if response.status() == StatusCode::Ok {
        smol::io::copy(response, &mut file).await?;
        file.flush().await?;
        std::mem::drop(file);
        Command::new(download_path).arg("/norestart").arg("/quiet").status()?;
        Ok(())
    } else {
        Err(anyhow!("could not download file"))
    }
}

async fn is_installed(arch: Architecture, runtime: Runtime, dotnet_version: &DotnetVersion) -> Result<bool> {

    let version_req = VersionReq::parse(&dotnet_version.to_string())?;
    let runtime_path = match runtime {
        Runtime::Dotnet => "shared\\Microsoft.NETCore.App",
        Runtime::AspCore => "shared\\Microsoft.AspNetCore.App",
        Runtime::WindowsDesktop => "shared\\Microsoft.WindowsDesktop.App",
    };

    let root_path = get_root_install(arch);
    if !root_path.exists() {
        return Ok(false)
    }

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

fn is_vcruntime_installed(arch: Architecture) -> bool {
    let path = match (arch, is_syswow64()) {
        (Architecture::X64, true) => Path::new("C:\\Windows\\SysNative\\vcruntime140.dll"),
        (Architecture::X64, false) => Path::new("C:\\Windows\\System32\\vcruntime140.dll"),
        (Architecture::X86, true) => Path::new("C:\\Windows\\System32\\vcruntime140.dll"),
        (Architecture::X86, false) => Path::new("C:\\Windows\\SysWOW64\\vcruntime140.dll"),
    };

    path.exists()
}

fn download_url(arch: Architecture, runtime: Runtime, version: Version, product_version: &str) -> String {
    let arch = match arch {
        Architecture::X86 => "x86",
        Architecture::X64 => "x64",
    };

    match runtime {
        Runtime::Dotnet => format!(
            "{}/Runtime/{}/dotnet-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, arch
        ),
        Runtime::AspCore => format!(
            "{}/aspnetcore/Runtime/{}/aspnetcore-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, arch
        ),
        Runtime::WindowsDesktop => format!(
            "{}/Runtime/{}/windowsdesktop-runtime-{}-win-{}.exe",
            BASE_URL, version, product_version, arch
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

fn get_root_install(arch: Architecture) -> &'static Path {
    match (arch, is_64bit_os()) {
        (Architecture::X64, true) | (Architecture::X86, false) => Path::new("C:\\Program Files\\dotnet"),
        (Architecture::X86, true) => Path::new("C:\\Program Files (x86)\\dotnet"),
        _ => unreachable!()
    }
}

fn is_64bit_os() -> bool {
    std::env::var_os("PROCESSOR_ARCHITEW6432").is_some() || std::env::consts::ARCH == "x86_64"
}

fn is_syswow64() -> bool {
    std::env::var_os("PROCESSOR_ARCHITEW6432").is_some()
}

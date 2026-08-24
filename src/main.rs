use std::{
    fs::{self, File},
    io::BufReader,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{FixedOffset, TimeZone};
use clap::{Parser, Subcommand};
use exif::{DateTime, Exif, In, Tag, Value};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "immich-fix", verbatim_doc_comment)]
struct Cli {
    #[arg(long, env = "IMMICH_URL")]
    url: String,

    #[arg(long, env = "IMMICH_API_KEY")]
    api_key: String,

    #[arg(long, default_value = "manifest.json")]
    manifest: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read every asset in an album and record its id/path/tags. Read-only
    List {
        /// The temporary album's UUID, from the Immich UI URL
        #[arg(long)]
        album_id: Option<String>,
        /// The temporary album's name
        #[arg(long)]
        album_name: Option<String>,
    },
    /// Fix the original timestamp of each asset in the given album
    Fix {
        /// The temporary album's UUID, from the Immich UI URL
        #[arg(long)]
        album_id: Option<String>,
        /// The temporary album's name
        #[arg(long)]
        album_name: Option<String>,
        /// Path prefix as it appears in Immich's `originalPath`
        #[arg(long, default_value = "/data")]
        container_prefix: String,
        /// The equivalent real path prefix on the machine
        #[arg(long)]
        host_prefix: String,
        /// Actually change the timestamp
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Serialize, Deserialize, Clone)]
struct AssetRecord {
    id: String,
    original_path: String,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    assets: Vec<AssetRecord>,
}

// ---- raw Immich API responses ----------------------------------------------

#[derive(Deserialize)]
struct AlbumResponse {
    id: String,
    #[allow(dead_code)]
    #[serde(rename = "albumName")]
    name: String,
    #[serde(rename = "assetCount")]
    asset_count: u64,
}

#[derive(Deserialize)]
struct AlbumSearchResponse {
    assets: Assets,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct Assets {
    total: u64,
    count: u64,
    items: Vec<AssetDetail>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct AlbumAsset {
    id: String,
}

#[derive(Deserialize)]
struct AssetDetail {
    id: String,
    #[serde(rename = "originalPath")]
    original_path: String,
    #[serde(rename = "originalFileName")]
    original_filename: String,
}

// ---- thin HTTP client ------------------------------------------------------

struct Client {
    http: reqwest::blocking::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    fn new(base_url: String, api_key: String) -> Self {
        Self {
            http: reqwest::blocking::Client::new(),
            base_url,
            api_key,
        }
    }

    #[allow(dead_code)]
    fn get(&self, path: &str) -> Result<reqwest::blocking::Response> {
        self.http
            .get(format!("{}/{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("Accept", "application/json")
            .query(&[("name", "")])
            .send()
            .context("request failed")
    }

    fn get_query<T: Serialize + ?Sized>(
        &self,
        path: &str,
        query: &T,
    ) -> Result<reqwest::blocking::Response> {
        self.http
            .get(format!("{}/{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("Accept", "application/json")
            .query(query)
            .send()
            .context("request failed")
    }

    fn post(&self, path: &str, body: &serde_json::Value) -> Result<reqwest::blocking::Response> {
        self.http
            .post(format!("{}/{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body)
            .send()
            .context("request failed")
    }

    #[allow(dead_code)]
    fn put(&self, path: &str, body: &serde_json::Value) -> Result<reqwest::blocking::Response> {
        self.http
            .put(format!("{}/{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body)
            .send()
            .context("request failed")
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let manifest_path = cli.manifest.clone();
    let client = Client::new(cli.url.trim_end_matches('/').to_string(), cli.api_key);

    match cli.command {
        Command::List {
            album_id,
            album_name,
        } => cmd_list(
            &client,
            album_id.as_deref(),
            album_name.as_deref(),
            &manifest_path,
        ),
        Command::Fix {
            album_id,
            album_name,
            apply,
            container_prefix,
            host_prefix,
        } => cmd_fix(
            &client,
            album_id.as_deref(),
            album_name.as_deref(),
            apply,
            &container_prefix,
            &host_prefix,
        ),
    }
}

// ---- list ------------------------------------------------------------------

fn cmd_list(
    client: &Client,
    album_id: Option<&str>,
    album_name: Option<&str>,
    manifest_path: &Path,
) -> Result<()> {
    if album_id.is_none() && album_name.is_none() {
        bail!("either album_id or album_name must be provided");
    }

    let (album_id, size) = get_album_id_and_size(client, album_id, album_name)?;
    let asset_details = get_asset_details(client, &album_id, size)?;

    let mut records = Vec::new();
    for detail in asset_details {
        records.push(AssetRecord {
            id: detail.id,
            original_path: detail.original_path,
        });
    }

    let manifest = Manifest { assets: records };
    fs::write(manifest_path, serde_json::to_string_pretty(&manifest)?)
        .context("failed to write manifest")?;
    println!(
        "\nWrote {} assets to {}",
        manifest.assets.len(),
        manifest_path.display()
    );
    Ok(())
}

// ---- fix timestamps -----------------------------------------------

fn cmd_fix(
    client: &Client,
    album_id: Option<&str>,
    album_name: Option<&str>,
    apply: bool,
    container_prefix: &str,
    host_prefix: &str,
) -> anyhow::Result<()> {
    if album_id.is_none() && album_name.is_none() {
        anyhow::bail!("must provide either album_id or album_name");
    }

    let (album_id, album_size) = get_album_id_and_size(client, album_id, album_name)?;
    let asset_details = get_asset_details(client, &album_id, album_size)?;

    let mut success = 0;
    for asset in asset_details.iter() {
        if let Ok((original_timestamp, offset)) =
            get_original_timestamp(container_prefix, host_prefix, asset)
        {
            let timestamp_string = get_datetime_string(original_timestamp, offset);
            let json = serde_json::json!({ "dateTimeOriginal": timestamp_string });

            if !apply {
                if success == 0 {
                    let request = client
                        .http
                        .put(format!("{}/assets/{}", client.base_url, asset.id))
                        .header("x-api-key", &client.api_key)
                        .header("Content-Type", "application/json")
                        .header("Accept", "application/json")
                        .json(&json)
                        .build()
                        .ok();
                    if let Some(request) = request {
                        println!("example request: {:?}", request);
                    }
                    return Ok(());
                }

                println!("Would update timestamp here to: {}", timestamp_string);
            }
            success += 1;
        } else {
            println!(
                "Failed to get timestamp for {} - {}",
                asset.original_filename, asset.original_path
            );
        }
    }

    println!(
        "Successfully updated [{}/{}] assets",
        success,
        asset_details.len()
    );

    Ok(())
}

// ---- helper methods --------------------------------------------------------

fn get_album_id_and_size(
    client: &Client,
    album_id: Option<&str>,
    album_name: Option<&str>,
) -> anyhow::Result<(String, usize)> {
    let mut size = 1000;
    let fetched_album_id: String;
    match album_id {
        Some(id) => Ok((id.to_string(), size)),
        None => {
            let name = album_name
                .ok_or_else(|| anyhow::anyhow!("must provide either album_id or album_name"))?;
            let resp = client.get_query(&format!("albums"), &[("name", name)])?;
            if !resp.status().is_success() {
                bail!(
                    "failed to fetch album: {} - {}",
                    resp.status(),
                    resp.text().unwrap_or_default()
                );
            }
            println!("{:?}", resp);
            // println!(
            //     "{}",
            //     resp.text().unwrap_or("failed to unwrap text".to_string())
            // );
            let albums: Vec<AlbumResponse> = resp.json()?;
            if albums.is_empty() {
                bail!("no albums found");
            }
            if albums.len() > 1 {
                bail!("multiple albums found: {}", albums.len());
            }
            let album = &albums[0];
            fetched_album_id = album.id.clone();
            size = album.asset_count as usize;
            Ok((fetched_album_id, size))
        }
    }
}

fn get_asset_details(
    client: &Client,
    album_id: &str,
    size: usize,
) -> anyhow::Result<Vec<AssetDetail>> {
    let body = serde_json::json!({ "albumIds": [album_id], "page": 1, "size": size});
    let resp = client.post("search/metadata", &body)?;
    if !resp.status().is_success() {
        bail!(
            "failed to fetch album: {} - {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    let album: AlbumSearchResponse = resp.json().context("unexpected album response shape")?;
    println!(
        "Album has {} assets. Fetching details for each...",
        album.assets.items.len()
    );

    if album.assets.items.len() != size {
        println!(
            "Warning: fetched {} assets, expected {}",
            album.assets.items.len(),
            size
        );
        // TODO need to handle pagination
    }
    Ok(album.assets.items)
}

fn get_original_timestamp(
    container_prefix: &str,
    host_prefix: &str,
    asset: &AssetDetail,
) -> Result<(DateTime, Option<Offset>)> {
    if !asset.original_path.starts_with(container_prefix) {
        bail!(
            " !! {} doesn't start with the given container prefix, original_path: {}",
            asset.original_filename,
            asset.original_path
        );
    }

    let host_path = asset
        .original_path
        .replacen(container_prefix, host_prefix, 1);
    // let sidecar = format!("{host_path}.xmp");

    let actual_path = Path::new(&host_path);
    if actual_path.exists() {
        // println!("Found file for {} - {}", asset.original_filename, host_path);
        let file = File::open(&actual_path)?;
        let mut bufreader = BufReader::new(&file);
        let exifreader = exif::Reader::new();
        let exif = exifreader.read_from_container(&mut bufreader)?;

        match exif.get_field(Tag::DateTimeOriginal, In::PRIMARY) {
            Some(time)
                if let Value::Ascii(ref vec) = time.value
                    && !vec.is_empty() =>
            {
                if let Ok(datetime) = DateTime::from_ascii(&vec[0]) {
                    let offset = get_offset(&exif);
                    return Ok((datetime, offset));
                }
                bail!("no value for DateTimeOriginal")
            }
            Some(_) => bail!("unsupported value type for DateTimeOriginal"),
            None => bail!("no DateTimeOriginal field found"),
        }
    } else {
        println!(
            " !! No file found for {}, filepath: {}",
            asset.original_filename, host_path
        );
        bail!("no file found");
    }
}

fn get_datetime_string(datetime: DateTime, offset: Option<Offset>) -> String {
    let hour = 3600;
    if let Some(offset) = offset
        && offset.hours > 0
    {
        if offset.is_negative {
            FixedOffset::west_opt(offset.hours as i32 * hour)
                .unwrap()
                .with_ymd_and_hms(
                    datetime.year.into(),
                    datetime.month.into(),
                    datetime.day.into(),
                    datetime.hour.into(),
                    datetime.minute.into(),
                    datetime.second.into(),
                )
                .unwrap()
                .to_rfc3339()
        } else {
            FixedOffset::east_opt(offset.hours as i32 * hour)
                .unwrap()
                .with_ymd_and_hms(
                    datetime.year.into(),
                    datetime.month.into(),
                    datetime.day.into(),
                    datetime.hour.into(),
                    datetime.minute.into(),
                    datetime.second.into(),
                )
                .unwrap()
                .to_rfc3339()
        }
    } else {
        format!(
            "{}-{:02}-{:02}T{:02}:{:02}:{:02}",
            datetime.year,
            datetime.month,
            datetime.day,
            datetime.hour,
            datetime.minute,
            datetime.second,
        )
    }
}

fn get_offset(exif: &Exif) -> Option<Offset> {
    match exif.get_field(Tag::OffsetTimeOriginal, In::PRIMARY) {
        Some(f) => {
            if let Value::Ascii(ref vec) = f.value
                && !vec.is_empty()
            {
                return parse_offset(&vec[0]).ok();
            }
        }
        None => (),
    };
    None
}

fn parse_offset(data: &[u8]) -> Result<Offset> {
    if data.is_empty() || data == b"   :  " {
        bail!("offset is empty or invalid");
    }

    let is_negative = data[0] == b'-';
    let num_start = match data {
        [b'-', ..] | [b'+', ..] => 1,
        _ => 0,
    };

    if data.len() - num_start != 5 {
        bail!(
            "offset length is invalid, length {}, numstart {}",
            data.len(),
            num_start
        );
    }

    let hours: usize = str::from_utf8(&data[num_start..num_start + 2])?.parse()?;
    let minutes: usize = str::from_utf8(&data[num_start + 3..num_start + 5])?.parse()?;

    Ok(Offset {
        hours,
        minutes,
        is_negative,
    })
}

#[derive(Debug, Clone, PartialEq)]
struct Offset {
    hours: usize,
    minutes: usize,
    is_negative: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_offset() {
        assert_eq!(
            parse_offset(b"+01:00").unwrap(),
            Offset {
                hours: 1,
                minutes: 0,
                is_negative: false,
            }
        );
        assert_eq!(
            parse_offset(b"-02:30").unwrap(),
            Offset {
                hours: 2,
                minutes: 30,
                is_negative: true,
            }
        );
        assert!(parse_offset(b"").is_err());
        assert!(parse_offset(b"+").is_err());
        assert!(parse_offset(b"+0").is_err());
        assert!(parse_offset(b"+01").is_err());
        assert!(parse_offset(b"+01:").is_err());
        assert!(parse_offset(b"+01:0").is_err());
        assert!(parse_offset(b"+01:00:").is_err());
    }

    #[test]
    fn get_chrono_time() {
        assert_eq!(
            get_datetime_string(
                DateTime {
                    year: 2026,
                    month: 2,
                    day: 3,
                    hour: 4,
                    minute: 5,
                    second: 6,
                    nanosecond: None,
                    offset: None,
                },
                Some(Offset {
                    hours: 0,
                    minutes: 0,
                    is_negative: false
                })
            ),
            "2026-02-03T04:05:06"
        );

        assert_eq!(
            get_datetime_string(
                DateTime {
                    year: 2026,
                    month: 2,
                    day: 3,
                    hour: 4,
                    minute: 5,
                    second: 6,
                    nanosecond: None,
                    offset: None,
                },
                None
            ),
            "2026-02-03T04:05:06"
        );

        assert_eq!(
            get_datetime_string(
                DateTime {
                    year: 2026,
                    month: 2,
                    day: 3,
                    hour: 4,
                    minute: 5,
                    second: 6,
                    nanosecond: None,
                    offset: None,
                },
                Some(Offset {
                    hours: 5,
                    minutes: 0,
                    is_negative: false
                })
            ),
            "2026-02-03T04:05:06+05:00"
        );

        assert_eq!(
            get_datetime_string(
                DateTime {
                    year: 2026,
                    month: 2,
                    day: 3,
                    hour: 4,
                    minute: 5,
                    second: 6,
                    nanosecond: None,
                    offset: None,
                },
                Some(Offset {
                    hours: 5,
                    minutes: 0,
                    is_negative: true
                })
            ),
            "2026-02-03T04:05:06-05:00"
        );
    }
}

//! `reverything-service query <text>`: searches through a running service, for testing the
//! pipe without the UI.

use std::time::Instant;

use eyre::{bail, Result};
use reverything_protocol::{Client, Request, Response, Sort};

pub fn run(query: &str) -> Result<()> {
    let t = Instant::now();
    let mut client = Client::connect()?;
    println!("connected in {:?}", t.elapsed());

    if let Ok(volumes) = std::env::var("RV_QUERY_VOLUMES") {
        // Changes which volumes are indexed, e.g. RV_QUERY_VOLUMES=CD
        let volumes = volumes.chars().map(|c| c.to_ascii_uppercase()).collect();
        match client.request(&Request::SetVolumes { volumes })? {
            Response::Done => println!("volumes changed"),
            other => bail!("Unexpected response {:?}", other),
        }
    }

    let t = Instant::now();
    match client.request(&Request::Status)? {
        Response::Status(status) => {
            if std::env::var_os("RV_QUERY_STATUS").is_some() {
                println!("status in {:?}: {:#?}", t.elapsed(), status);
            } else {
                println!("status in {:?}", t.elapsed());
            }
        }
        other => bail!("Unexpected response {:?}", other),
    }

    let t = Instant::now();
    let (search, total, took_us) = match client.request(&Request::Search {
        query: query.to_string(),
        sort: Sort::default(),
        files: std::env::var_os("RV_QUERY_NO_FILES").is_none(),
        folders: std::env::var_os("RV_QUERY_NO_FOLDERS").is_none(),
    })? {
        Response::Search {
            search,
            total,
            took_us,
        } => (search, total, took_us),
        other => bail!("Unexpected response {:?}", other),
    };
    println!(
        "{} results for {:?}, search took {} us, round trip {:?}",
        total,
        query,
        took_us,
        t.elapsed()
    );

    let t = Instant::now();
    match client.request(&Request::Rows {
        search,
        start: 0,
        count: 50,
    })? {
        Response::Rows { rows, .. } => {
            println!("{} rows in {:?}", rows.len(), t.elapsed());
            for row in rows.iter().take(20) {
                println!(
                    "  {}\\{}  {} B",
                    row.folder.trim_end_matches('\\'),
                    row.name,
                    row.size
                );
            }
        }
        other => bail!("Unexpected response {:?}", other),
    }
    Ok(())
}

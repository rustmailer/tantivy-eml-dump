use std::{collections::HashMap, path::PathBuf, process};

use chrono::Local;
use clap::Parser;
use tantivy::{
    Index, TantivyDocument, Term,
    collector::{Count, TopDocs},
    query::{AllQuery, TermQuery},
    schema::{IndexRecordOption, Value},
};
use tokio::io::AsyncWriteExt;

#[derive(Parser, Debug)]
#[command(
    name = "eml-dumper",
    author = "rustmailer",
    about = "A specific recovery tool to extract email data from Bichon v0.3.7 and earlier data directories into an mbox file"
)]
pub struct BichonCli {
    /// Absolute path to the BICHON_DATA_DIR of v0.3.7 or earlier
    #[arg(
        short,
        long,
        value_name = "DIR",
        required = true,
        help = "The absolute path to the legacy BICHON_DATA_DIR (v0.3.7 or earlier)"
    )]
    pub data_dir: PathBuf,

    /// Absolute path to the output directory
    #[arg(
        short,
        long,
        value_name = "DIR",
        required = true,
        help = "The absolute path to the output directory where the mbox file will be saved"
    )]
    pub output_dir: PathBuf,
}

pub const F_ID: &str = "id";
pub const F_ACCOUNT_ID: &str = "account_id";
pub const F_MAILBOX_ID: &str = "mailbox_id";
pub const F_EML: &str = "eml";

#[tokio::main]
async fn main() {
    let cli = BichonCli::parse();
    let data_dir = &cli.data_dir;
    let output_dir = &cli.output_dir;

    // Check if the path exists
    if !data_dir.exists() {
        eprintln!("Error: Path '{}' does not exist.", data_dir.display());
        process::exit(1);
    }

    if !data_dir.is_dir() {
        eprintln!("Error: Path '{}' is not a directory.", data_dir.display());
        process::exit(1);
    }

    // Optional: Validate that the path is absolute
    if !data_dir.is_absolute() {
        eprintln!(
            "Error: Path '{}' must be an absolute path.",
            data_dir.display()
        );
        process::exit(1);
    }

    if !output_dir.exists() {
        eprintln!(
            "Error: Output directory '{}' does not exist.",
            output_dir.display()
        );
        process::exit(1);
    }
    if !output_dir.is_dir() {
        eprintln!(
            "Error: Output path '{}' is not a directory.",
            output_dir.display()
        );
        process::exit(1);
    }
    if !output_dir.is_absolute() {
        eprintln!(
            "Error: Output directory '{}' must be an absolute path.",
            output_dir.display()
        );
        process::exit(1);
    }

    let index = match Index::open_in_dir(data_dir) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!(
                "Error: Failed to open Tantivy index in directory '{}'. Details: {}",
                data_dir.display(),
                e
            );
            process::exit(1);
        }
    };

    let schema = index.schema();
    let f_account_id = schema
        .get_field(F_ACCOUNT_ID)
        .expect("f_account_id field missing");
    let f_eml = schema.get_field(F_EML).expect("f_eml field missing");

    let reader = match index.reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: Failed to create index reader. Details: {}", e);
            process::exit(1);
        }
    };
    let searcher = reader.searcher();

    let query = AllQuery;

    let total_count = match searcher.search(&query, &Count) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("Error: Failed to count documents. Details: {}", e);
            process::exit(1);
        }
    };

    println!("Total documents found in index: {}", total_count);

    let mut account_stats: HashMap<u64, usize> = HashMap::new();

    let limit = 5000; // Define chunk size for pagination
    let mut offset = 0;

    while offset < total_count {
        let top_docs_collector = TopDocs::with_limit(limit).and_offset(offset);

        let top_docs = match searcher.search(&query, &top_docs_collector) {
            Ok(docs) => docs,
            Err(e) => {
                eprintln!(
                    "Error: Failed to query batch at offset {}. Details: {}",
                    offset, e
                );
                process::exit(1);
            }
        };

        if top_docs.is_empty() {
            break;
        }

        for (_score, doc_address) in top_docs {
            if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                if let Some(field_value) = doc.get_first(f_account_id) {
                    if let Some(val) = field_value.as_u64() {
                        *account_stats.entry(val).or_insert(0) += 1;
                    }
                }
            }
        }

        offset += limit;
    }

    for (&account_id, &total_records) in &account_stats {
        println!(
            "Processing Account ID: {} ({} records)",
            account_id, total_records
        );

        // Define output mbox file path for the account
        let mbox_file = output_dir.join(format!("account_{}.mbox", account_id));

        // Open file in append mode (creates if not exists)
        let mut file = match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&mbox_file)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(" ✘ Failed to open file '{}': {}", &mbox_file.display(), e);
                return;
            }
        };

        // TermQuery for specific account_id filtering
        let term = Term::from_field_u64(f_account_id, account_id);
        let account_query = TermQuery::new(term, IndexRecordOption::Basic);

        let export_limit = 2000;
        let mut export_offset = 0usize;

        let date_dt = Local::now();
        let date_str = date_dt.format("%a %b %e %H:%M:%S %Y").to_string();
        let from_line = format!("From sender@example.com {}\n", date_str);

        while export_offset < total_records {
            let top_docs = match searcher.search(
                &account_query,
                &TopDocs::with_limit(export_limit).and_offset(export_offset),
            ) {
                Ok(docs) => docs,
                Err(e) => {
                    eprintln!(
                        "Error: Failed to query account {} at offset {}. Details: {}",
                        account_id, export_offset, e
                    );
                    process::exit(1);
                }
            };

            if top_docs.is_empty() {
                break;
            }

            for (_score, doc_address) in top_docs {
                if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                    if let Some(field_value) = doc.get_first(f_eml) {
                        if let Some(eml_bytes) = field_value.as_bytes() {
                            let mut final_buffer =
                                Vec::with_capacity(from_line.len() + eml_bytes.len() + 2);
                            final_buffer.extend_from_slice(from_line.as_bytes());
                            final_buffer.extend_from_slice(eml_bytes);
                            final_buffer.extend_from_slice(b"\n\n");

                            if let Err(e) = file.write_all(&final_buffer).await {
                                eprintln!(" ✘ IO Error: Failed to write to mbox: {}", e);
                                return;
                            }
                        }
                    }
                }
            }

            export_offset += export_limit;
            println!(
                " - Progress: exported {} / {} records",
                export_offset.min(total_records),
                total_records
            );
        }

        if let Err(e) = file.shutdown().await {
            eprintln!(
                " ✘ Warning: Failed to flush and shutdown file properly: {}",
                e
            );
        }

        println!(
            "Successfully generated mbox file for account {}: {:?}",
            account_id, &mbox_file
        );
    }
}

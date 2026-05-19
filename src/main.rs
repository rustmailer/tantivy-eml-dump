use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    process,
    rc::Rc,
};

use base64::Engine;
use chrono::Local;
use clap::Parser;
use console::style;
use dialoguer::Select;
use indicatif::{ProgressBar, ProgressStyle};
use itertools::Itertools;
use native_db::Builder;
use tantivy::{
    Index, TantivyDocument, Term,
    collector::{Count, TopDocs},
    query::AllQuery,
    query::TermQuery,
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
        required = true,
        help = "The absolute path to the legacy BICHON_DATA_DIR (v0.3.7 or earlier)"
    )]
    pub data_dir: PathBuf,

    /// Absolute path to the output directory
    #[arg(
        short,
        long,
        required = true,
        help = "The absolute path to the output directory where the mbox file will be saved"
    )]
    pub output_dir: PathBuf,

    /// Absolute path to the Bichon root directory
    #[arg(
        short,
        long,
        help = "The absolute path to the legacy BICHON_ROOT_DIR (v0.3.7 or earlier). If not provided, exports will use numeric IDs."
    )]
    pub root_dir: Option<PathBuf>,
}

pub const F_ID: &str = "id";
pub const F_ACCOUNT_ID: &str = "account_id";
pub const F_MAILBOX_ID: &str = "mailbox_id";
pub const F_EML: &str = "eml";

fn validate_dir(path: &PathBuf, label: &str) {
    if !path.exists() {
        eprintln!("Error: {} '{}' does not exist.", label, path.display());
        process::exit(1);
    }
    if !path.is_dir() {
        eprintln!("Error: {} '{}' is not a directory.", label, path.display());
        process::exit(1);
    }
    if !path.is_absolute() {
        eprintln!(
            "Error: {} '{}' must be an absolute path.",
            label,
            path.display()
        );
        process::exit(1);
    }
}

fn try_open_native(
    root: Option<&PathBuf>,
) -> Option<(Vec<AccountV3>, HashMap<u64, (u64, String)>)> {
    let root = root?;
    let meta_db_path = root.join("meta.db");
    let mailbox_db_path = root.join("mailbox.db");

    let meta_db = match Builder::new()
        .set_cache_size(67_108_864)
        .create(&META_MODELS, &meta_db_path)
    {
        Ok(db) => Rc::new(db),
        Err(_) => return None,
    };

    let mailbox_db = match Builder::new()
        .set_cache_size(67_108_864)
        .create(&MAILBOX_MODELS, &mailbox_db_path)
    {
        Ok(db) => Rc::new(db),
        Err(_) => return None,
    };

    let accounts: Vec<AccountV3> = list_all_impl(&meta_db).ok()?;
    let mailboxes: Vec<MailBox> = list_all_impl(&mailbox_db).ok()?;

    let mailbox_info: HashMap<u64, (u64, String)> = mailboxes
        .iter()
        .map(|m| (m.id, (m.account_id, m.name.clone())))
        .collect();

    Some((accounts, mailbox_info))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = BichonCli::parse();
    let data_dir = &cli.data_dir;
    let output_dir = &cli.output_dir;
    let bichon_root_dir = &cli.root_dir;

    validate_dir(data_dir, "Bichon Data directory");
    validate_dir(output_dir, "Output directory");
    if let Some(root) = bichon_root_dir {
        validate_dir(root, "Bichon root directory");
    }

    let native = try_open_native(bichon_root_dir.as_ref());

    // ── Open Tantivy index ────────────────────────────────────────────────

    let index = match Index::open_in_dir(data_dir) {
        Ok(idx) => idx,
        Err(e) => {
            eprintln!(
                "Error: Failed to open Tantivy index in '{}': {}",
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
    let f_mailbox_id = schema
        .get_field(F_MAILBOX_ID)
        .expect("f_mailbox_id field missing");
    let f_eml = schema.get_field(F_EML).expect("f_eml field missing");

    let reader = match index.reader() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Error: Failed to create index reader: {}", e);
            process::exit(1);
        }
    };
    let searcher = reader.searcher();

    // ── Branch: native mode vs fallback mode ──────────────────────────────

    match native {
        Some((accounts, mailbox_info)) => {
            export_with_native(
                &searcher,
                &accounts,
                &mailbox_info,
                f_account_id,
                f_mailbox_id,
                f_eml,
                output_dir,
            )
            .await;
        }
        None => {
            // If user explicitly provided --root-dir but native dbs failed, ask before fallback
            if bichon_root_dir.is_some() {
                eprintln!(
                    "Warning: native databases (meta.db / mailbox.db) not available. Continue with numeric export? [y/N]"
                );
                let mut input = String::new();
                io::stdin().read_line(&mut input).unwrap();
                if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
                    eprintln!("Aborted.");
                    process::exit(0);
                }
            }
            export_fallback(&searcher, f_account_id, f_eml, output_dir).await;
        }
    }
}

async fn export_with_native(
    searcher: &tantivy::Searcher,
    accounts: &[AccountV3],
    mailbox_info: &HashMap<u64, (u64, String)>,
    f_account_id: tantivy::schema::Field,
    f_mailbox_id: tantivy::schema::Field,
    f_eml: tantivy::schema::Field,
    output_dir: &PathBuf,
) {
    // ── Interactive account selection ──────────────────────────────────

    if accounts.is_empty() {
        eprintln!("No accounts found in meta.db.");
        process::exit(1);
    }

    let selected_account_id: u64 = if accounts.len() == 1 {
        let a = &accounts[0];
        println!(
            "{} {} | {}",
            style("Only one account found:").bold(),
            a.email,
            a.name.as_deref().unwrap_or("(no name)")
        );
        a.id
    } else {
        let items: Vec<String> = accounts
            .iter()
            .map(|a| {
                format!(
                    "{} | {}",
                    a.email,
                    a.name.as_deref().unwrap_or("(no name)")
                )
            })
            .collect();

        let selection = Select::new()
            .with_prompt("Select an account to export")
            .items(&items)
            .default(0)
            .interact()
            .unwrap();
        accounts[selection].id
    };

    let selected_account = accounts
        .iter()
        .find(|a| a.id == selected_account_id)
        .unwrap();

    println!(
        "\n{} {}",
        style("Exporting emails for:").bold(),
        style(&selected_account.email).cyan()
    );

    let term = Term::from_field_u64(f_account_id, selected_account_id);
    let account_query = TermQuery::new(term, IndexRecordOption::Basic);

    let total_count = match searcher.search(&account_query, &Count) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("Error: Failed to count documents: {}", e);
            process::exit(1);
        }
    };

    println!("Total documents: {}", style(total_count).yellow());

    if total_count == 0 {
        println!("No emails to export.");
        return;
    }

    let date_dt = Local::now();
    let date_str = date_dt.format("%a %b %e %H:%M:%S %Y").to_string();
    let email = &selected_account.email;
    let safe_email = email.replace(['@', '/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");

    let mbox_filename = format!("{}.mbox", safe_email);
    let mbox_file = output_dir.join(&mbox_filename);

    println!(
        "Exporting {} -> {} ({} records)",
        style(email).cyan(),
        mbox_filename,
        style(total_count).yellow()
    );

    let mut file = match tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&mbox_file)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(" ✘ Failed to open '{}': {}", &mbox_file.display(), e);
            return;
        }
    };

    let from_line = format!("From {} {}\n", email, date_str);

    let segment_readers = searcher.segment_readers();
    let pb = ProgressBar::new(total_count as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{bar:40} {pos}/{len} [{elapsed_precise}]")
            .unwrap(),
    );

    let export_limit = 2000;
    let mut export_offset = 0usize;

    while export_offset < total_count {
        let top_docs = match searcher.search(
            &account_query,
            &TopDocs::with_limit(export_limit).and_offset(export_offset),
        ) {
            Ok(docs) => docs,
            Err(e) => {
                eprintln!("Error: Failed to query at offset {}: {}", export_offset, e);
                process::exit(1);
            }
        };

        if top_docs.is_empty() {
            break;
        }

        for (_score, doc_address) in top_docs {
            let segment_reader = &segment_readers[doc_address.segment_ord as usize];
            let doc_mid = segment_reader
                .fast_fields()
                .u64(searcher.index().schema().get_field_name(f_mailbox_id))
                .expect("mailbox_id fast field")
                .values
                .get_val(doc_address.doc_id);

            let mailbox_name = mailbox_info
                .get(&doc_mid)
                .map(|(_, name)| name.clone());

            let metadata_header = build_metadata_header(&BichonMetadata {
                account_email: Some(email.clone()),
                mailbox_name,
            });

            if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                if let Some(field_value) = doc.get_first(f_eml) {
                    if let Some(eml_bytes) = field_value.as_bytes() {
                        let mut final_buffer = Vec::with_capacity(
                            from_line.len() + metadata_header.len() + eml_bytes.len() + 2,
                        );
                        final_buffer.extend_from_slice(from_line.as_bytes());
                        final_buffer.extend_from_slice(metadata_header.as_bytes());
                        final_buffer.extend_from_slice(eml_bytes);
                        final_buffer.extend_from_slice(b"\n\n");

                        if let Err(e) = file.write_all(&final_buffer).await {
                            eprintln!(" ✘ IO Error: {}", e);
                            return;
                        }
                        pb.inc(1);
                    }
                }
            }
        }

        export_offset += export_limit;
    }

    if let Err(e) = file.shutdown().await {
        eprintln!(" ✘ Warning: Failed to flush file: {}", e);
    }

    pb.finish_with_message(format!("Done: {}", mbox_filename));
}

async fn export_fallback(
    searcher: &tantivy::Searcher,
    f_account_id: tantivy::schema::Field,
    f_eml: tantivy::schema::Field,
    output_dir: &PathBuf,
) {
    println!("Running in fallback mode — exporting all accounts by numeric ID.\n");

    let total_count = match searcher.search(&AllQuery, &Count) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("Error: Failed to count documents: {}", e);
            process::exit(1);
        }
    };

    println!("Total documents found in index: {}", total_count);

    let mut account_stats: HashMap<u64, usize> = HashMap::new();
    let limit = 5000;
    let mut offset = 0;

    let segment_readers = searcher.segment_readers();

    let scan_pb = ProgressBar::new(total_count as u64);
    scan_pb.set_style(
        ProgressStyle::default_bar()
            .template("{bar:40} {pos}/{len} {msg}")
            .unwrap(),
    );
    scan_pb.set_message("Scanning account distribution...");

    while offset < total_count {
        let top_docs =
            match searcher.search(&AllQuery, &TopDocs::with_limit(limit).and_offset(offset)) {
                Ok(docs) => docs,
                Err(e) => {
                    eprintln!("Error: Failed to query at offset {}: {}", offset, e);
                    process::exit(1);
                }
            };

        if top_docs.is_empty() {
            break;
        }

        for (_score, doc_address) in top_docs {
            let segment_reader = &segment_readers[doc_address.segment_ord as usize];
            let acc_id = segment_reader
                .fast_fields()
                .u64(searcher.index().schema().get_field_name(f_account_id))
                .expect("account_id fast field")
                .values
                .get_val(doc_address.doc_id);
            *account_stats.entry(acc_id).or_insert(0) += 1;
        }

        offset += limit;
        scan_pb.set_position(offset.min(total_count) as u64);
    }

    scan_pb.finish_and_clear();

    let date_dt = Local::now();
    let date_str = date_dt.format("%a %b %e %H:%M:%S %Y").to_string();

    for (&account_id, &total_records) in &account_stats {
        let mbox_file = output_dir.join(format!("account_{}.mbox", account_id));

        println!(
            "Exporting account {} ({} records) -> {}",
            account_id,
            total_records,
            mbox_file.display()
        );

        let mut file = match tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&mbox_file)
            .await
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!(" ✘ Failed to open '{}': {}", &mbox_file.display(), e);
                return;
            }
        };

        let from_line = format!("From sender@example.com {}\n", date_str);
        let term = Term::from_field_u64(f_account_id, account_id);
        let account_query = TermQuery::new(term, IndexRecordOption::Basic);

        let pb = ProgressBar::new(total_records as u64);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{bar:40} {pos}/{len} [{elapsed_precise}]")
                .unwrap(),
        );

        let export_limit = 2000;
        let mut export_offset = 0usize;

        while export_offset < total_records {
            let top_docs = match searcher.search(
                &account_query,
                &TopDocs::with_limit(export_limit).and_offset(export_offset),
            ) {
                Ok(docs) => docs,
                Err(e) => {
                    eprintln!("Error: Failed to query at offset {}: {}", export_offset, e);
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
                                eprintln!(" ✘ IO Error: {}", e);
                                return;
                            }
                            pb.inc(1);
                        }
                    }
                }
            }

            export_offset += export_limit;
        }

        if let Err(e) = file.shutdown().await {
            eprintln!(" ✘ Warning: Failed to flush file: {}", e);
        }

        pb.finish_with_message(format!("Done: account_{}.mbox", account_id));
    }
}

// ─── Native DB Models ───────────────────────────────────────────────────────
// Minimal model definitions matching Bichon's native_db schema.
// Only the fields needed for deserialization are included; business logic
// methods from the original Bichon codebase are omitted.

use std::sync::LazyLock;

use native_db::*;
use native_model::{Model, native_model};
use serde::{Deserialize, Serialize};

// -- Account model (meta.db) -------------------------------------------------

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum AccountType {
    #[default]
    IMAP,
    NoSync,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum Encryption {
    #[default]
    Ssl,
    StartTls,
    None,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum AuthType {
    #[default]
    Password,
    OAuth2,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct AuthConfig {
    pub auth_type: AuthType,
    pub password: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub encryption: Encryption,
    pub auth: AuthConfig,
    pub use_proxy: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub enum Unit {
    #[default]
    Days,
    Months,
    Years,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct RelativeDate {
    pub unit: Unit,
    pub value: u32,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct DateSince {
    pub fixed: Option<String>,
    pub relative: Option<RelativeDate>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[native_model(id = 4, version = 1)]
#[native_db(primary_key(pk -> String))]
pub struct AccountV1 {
    #[secondary_key(unique)]
    pub id: u64,
    pub imap: Option<ImapConfig>,
    pub enabled: bool,
    pub email: String,
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub date_since: Option<DateSince>,
    pub folder_limit: Option<u32>,
    pub sync_folders: Option<Vec<String>>,
    pub account_type: AccountType,
    pub sync_interval_min: Option<i64>,
    pub known_folders: Option<std::collections::BTreeSet<String>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub use_proxy: Option<u64>,
}

impl AccountV1 {
    fn pk(&self) -> String {
        format!("{}_{}", self.created_at, self.id)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[native_model(id = 4, version = 2, from = AccountV1)]
#[native_db(primary_key(pk -> String))]
pub struct AccountV2 {
    #[secondary_key(unique)]
    pub id: u64,
    pub imap: Option<ImapConfig>,
    pub enabled: bool,
    pub email: String,
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub date_since: Option<DateSince>,
    pub folder_limit: Option<u32>,
    pub sync_folders: Option<Vec<String>>,
    pub account_type: AccountType,
    pub sync_interval_min: Option<i64>,
    pub known_folders: Option<std::collections::BTreeSet<String>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub use_proxy: Option<u64>,
    pub use_dangerous: bool,
    pub pgp_key: Option<String>,
}

impl AccountV2 {
    fn pk(&self) -> String {
        format!("{}_{}", self.created_at, self.id)
    }
}

impl From<AccountV1> for AccountV2 {
    fn from(value: AccountV1) -> Self {
        Self {
            id: value.id,
            imap: value.imap,
            enabled: value.enabled,
            email: value.email,
            name: value.name,
            capabilities: value.capabilities,
            date_since: value.date_since,
            folder_limit: value.folder_limit,
            sync_folders: value.sync_folders,
            account_type: value.account_type,
            sync_interval_min: value.sync_interval_min,
            known_folders: value.known_folders,
            created_at: value.created_at,
            updated_at: value.updated_at,
            use_proxy: value.use_proxy,
            use_dangerous: false,
            pgp_key: None,
        }
    }
}

impl From<AccountV2> for AccountV1 {
    fn from(value: AccountV2) -> Self {
        Self {
            id: value.id,
            imap: value.imap,
            enabled: value.enabled,
            email: value.email,
            name: value.name,
            capabilities: value.capabilities,
            date_since: value.date_since,
            folder_limit: value.folder_limit,
            sync_folders: value.sync_folders,
            account_type: value.account_type,
            sync_interval_min: value.sync_interval_min,
            known_folders: value.known_folders,
            created_at: value.created_at,
            updated_at: value.updated_at,
            use_proxy: value.use_proxy,
        }
    }
}

/// The current account model — matches Bichon's AccountV3 schema exactly.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[native_model(id = 4, version = 3, from = AccountV2)]
#[native_db(primary_key(pk -> String))]
pub struct AccountV3 {
    #[secondary_key(unique)]
    pub id: u64,
    pub imap: Option<ImapConfig>,
    pub enabled: bool,
    pub email: String,
    pub name: Option<String>,
    pub capabilities: Option<Vec<String>>,
    pub date_since: Option<DateSince>,
    pub date_before: Option<RelativeDate>,
    pub folder_limit: Option<u32>,
    pub sync_folders: Option<Vec<String>>,
    pub account_type: AccountType,
    pub sync_interval_min: Option<i64>,
    pub sync_batch_size: Option<u32>,
    pub known_folders: Option<std::collections::BTreeSet<String>>,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_by: u64,
    pub use_proxy: Option<u64>,
    pub use_dangerous: bool,
    pub pgp_key: Option<String>,
}

impl AccountV3 {
    fn pk(&self) -> String {
        format!("{}_{}", self.created_at, self.id)
    }
}

impl From<AccountV2> for AccountV3 {
    fn from(value: AccountV2) -> Self {
        Self {
            id: value.id,
            imap: value.imap,
            enabled: value.enabled,
            email: value.email,
            name: value.name,
            capabilities: value.capabilities,
            date_since: value.date_since,
            date_before: None,
            folder_limit: value.folder_limit,
            sync_folders: value.sync_folders,
            account_type: value.account_type,
            sync_interval_min: value.sync_interval_min,
            sync_batch_size: None,
            known_folders: value.known_folders,
            created_at: value.created_at,
            updated_at: value.updated_at,
            created_by: 1,
            use_proxy: value.use_proxy,
            use_dangerous: value.use_dangerous,
            pgp_key: value.pgp_key,
        }
    }
}

impl From<AccountV3> for AccountV2 {
    fn from(value: AccountV3) -> Self {
        Self {
            id: value.id,
            imap: value.imap,
            enabled: value.enabled,
            email: value.email,
            name: value.name,
            capabilities: value.capabilities,
            date_since: value.date_since,
            folder_limit: value.folder_limit,
            sync_folders: value.sync_folders,
            account_type: value.account_type,
            sync_interval_min: value.sync_interval_min,
            known_folders: value.known_folders,
            created_at: value.created_at,
            updated_at: value.updated_at,
            use_proxy: value.use_proxy,
            use_dangerous: value.use_dangerous,
            pgp_key: value.pgp_key,
        }
    }
}

// -- Mailbox model (mailbox.db) ----------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub enum AttributeEnum {
    NoInferiors,
    NoSelect,
    Marked,
    Unmarked,
    All,
    Archive,
    Drafts,
    Flagged,
    Junk,
    Sent,
    Trash,
    Extension,
    Unknown,
}

impl Default for AttributeEnum {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct Attribute {
    pub attr: AttributeEnum,
    pub extension: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[native_model(id = 1, version = 1)]
#[native_db]
pub struct MailBox {
    #[primary_key]
    pub id: u64,
    #[secondary_key]
    pub account_id: u64,
    pub name: String,
    pub delimiter: Option<String>,
    pub attributes: Vec<Attribute>,
    pub exists: u32,
    pub unseen: Option<u32>,
    pub uid_next: Option<u32>,
    pub uid_validity: Option<u32>,
}

/// Stub model required because mailbox.db was created with this model
/// registered. We never read from it — it just needs to exist so
/// native_db can open the database.
#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
#[native_model(id = 2, version = 1)]
#[native_db]
pub struct AccountRunningState {
    #[primary_key]
    pub account_id: u64,
}

// -- Static model sets -------------------------------------------------------

pub static META_MODELS: LazyLock<Models> = LazyLock::new(|| {
    let mut models = Models::new();
    models.define::<AccountV1>().expect("define AccountV1");
    models.define::<AccountV2>().expect("define AccountV2");
    models.define::<AccountV3>().expect("define AccountV3");
    models
});

pub static MAILBOX_MODELS: LazyLock<Models> = LazyLock::new(|| {
    let mut models = Models::new();
    models.define::<MailBox>().expect("define MailBox");
    models
        .define::<AccountRunningState>()
        .expect("define AccountRunningState");
    models
});

pub fn list_all_impl<T: ToInput + Clone + Send + 'static>(
    database: &Rc<Database<'static>>,
) -> native_db::db_type::Result<Vec<T>> {
    let r_transaction = database.r_transaction()?;
    let entities: Vec<T> = r_transaction.scan().primary()?.all()?.try_collect()?;
    Ok(entities)
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Deserialize, Serialize)]
pub struct BichonMetadata {
    pub account_email: Option<String>,
    pub mailbox_name: Option<String>,
}

pub fn parse_bichon_metadata(header_value: &str) -> Option<BichonMetadata> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(header_value.trim())
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

pub fn build_metadata_header(metadata: &BichonMetadata) -> String {
    let json = serde_json::to_vec(metadata).unwrap();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&json);
    format!("X-Bichon-Metadata: {}\n", encoded)
}

// let email_bytes = match response.bytes().await {
//         Ok(b) => b,
//         Err(e) => {
//             eprintln!(
//                 " ✘ Failed to read response body for {}: {}",
//                 &envelope.id, e
//             );
//             return false;
//         }
//     };

//     let date_dt = Utc.timestamp_opt(envelope.date / 1000, 0).unwrap();
//     let date_str = date_dt.format("%a %b %e %H:%M:%S %Y").to_string();
//     let from_line = format!("From {} {}\n", envelope.from.clone(), date_str);

//     let custom_header = build_metadata_header(BichonMetadata {
//         account_email: envelope.account_email,
//         mailbox_name: envelope.mailbox_name,
//         tags: envelope.tags,
//     });

//     let mut final_buffer =
//         Vec::with_capacity(from_line.len() + custom_header.len() + email_bytes.len() + 2);
//     final_buffer.extend_from_slice(from_line.as_bytes());
//     final_buffer.extend_from_slice(custom_header.as_bytes());
//     final_buffer.extend_from_slice(&email_bytes);
//     final_buffer.extend_from_slice(b"\n\n");

//     if let Err(e) = file.write_all(&final_buffer).await {
//         eprintln!(" ✘ IO Error: Failed to write to mbox: {}", e);
//         return false;
//     }

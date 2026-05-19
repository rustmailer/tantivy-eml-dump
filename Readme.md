# tantivy-eml-dump

**tantivy-eml-dump** extracts emails from legacy Bichon data directories (v0.3.7 and earlier) into **mbox** files by reading the EML data directly from the Tantivy index.

It has two operating modes:

- **If you provide `meta.db` and `mailbox.db`** (via `--root-dir`), it reads account and mailbox metadata, lets you pick an account interactively, and exports a single `{email}.mbox` per account.
- **If those databases are unavailable** (no `--root-dir`, or they can't be opened), it falls back to numeric IDs and exports `account_{id}.mbox` per account found in the index.

---

## Usage

### Option 1: Prebuilt binaries

Download the appropriate binary for your platform from the **Releases** page, then run:

```bash
./tantivy-eml-dump \
  --data-dir /path/to/index \
  --output-dir /path/to/output
```

### Option 2: Build from source

```bash
cargo build --release

./target/release/tantivy-eml-dump \
  --data-dir /path/to/index \
  --output-dir /path/to/output
```

---

## Arguments

| Argument | Required | Description |
|---|---|---|
| `--data-dir` | Yes | Path to the Tantivy index directory (Bichon's `BICHON_DATA_DIR`) |
| `--output-dir` | Yes | Path where mbox files will be written |
| `--root-dir` | No | Path to the Bichon root directory (contains `meta.db` and `mailbox.db`) |

---

## Modes

### With `meta.db` and `mailbox.db` (`--root-dir` provided and databases readable)

- Reads account list from `meta.db` and mailbox info from `mailbox.db`
- Lets you interactively select which account to export
- Output: one `.mbox` file per account — `{email}.mbox`
- Each email in the mbox carries an `X-Bichon-Metadata` header recording its `account_email` and `mailbox_name`

### Without native databases (`--root-dir` omitted or databases unreadable)

- Scans the Tantivy index to discover all account IDs
- Exports every account found, grouped by numeric `account_id`
- Output: `account_{account_id}.mbox`

> [!IMPORTANT]
> In this mode, account emails and mailbox names are unavailable — output files use numeric identifiers, and `X-Bichon-Metadata` headers are not written.

---

## `X-Bichon-Metadata` header

When `--root-dir` is provided, each exported email includes a custom header:

```
X-Bichon-Metadata: <base64-encoded JSON>
```

The decoded JSON payload:

```json
{
  "account_email": "user@example.com",
  "mailbox_name": "INBOX"
}
```

This allows downstream tools (such as `bichon-cli`) to recover the original account-to-mailbox association even though the mbox format itself does not carry that information.

### Importing into Bichon 1.x

When importing the generated mbox into a Bichon 1.x server via `bichon-cli`, select the import mode:

> **Use X-Bichon-Metadata header (Automatic)**

This tells `bichon-cli` to read the `X-Bichon-Metadata` header from each email and automatically assign the correct account and mailbox — no manual mapping required.

---

## When to use

Both modes are read-only and do not modify the source index. This tool is particularly useful when:

- `envelope` index is corrupted but the EML index is intact
- You need to export emails from a Bichon data directory without running the full Bichon service
- You are migrating from Bichon v0.3.7 to v1.x

---

## Notes

- Re-running appends to existing mbox files, which may create duplicates
- Only supports Bichon v0.3.7 and earlier
- Do not use network file systems (NFS, SMB) as the data directory — network instability can lead to index or data corruption

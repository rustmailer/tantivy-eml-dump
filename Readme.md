
# eml-dumper

**eml-dumper** is a recovery tool for extracting emails from legacy Bichon data directories (v0.3.7 and earlier) when metadata is corrupted but the **EML index remains intact**.

It is designed for cases where the `envelope` index or other system metadata is damaged.

---

## What it does

* Reads raw email data directly from the Tantivy index
* Bypasses broken metadata
* Exports emails into **mbox** files
* Groups output by `account_id`

---

## Usage

### Option 1: Use prebuilt binaries (recommended)

Download the appropriate binary for your platform from the **Releases** page, then run:

```bash id="m4q2sl"
./tantivy-eml-dump \
  --data-dir /absolute/path/to/BICHON_DATA_DIR \
  --output-dir /absolute/path/to/output
```

---

### Option 2: Build from source

```bash id="9z0j3g"
cargo build --release

./target/release/tantivy-eml-dump \
  --data-dir /absolute/path/to/BICHON_DATA_DIR \
  --output-dir /absolute/path/to/output
```

---

## Data Directory

* You can pass:

  * `/absolute/path/to/BICHON_DATA_DIR`
* Or, if not explicitly managed:

  * `BICHON_ROOT_DIR/eml`

---

## Output

* One `.mbox` file per account:

```
account_<account_id>.mbox
```

---

## When to use

* `envelope` index is corrupted
* Bichon metadata is broken
* **EML index is still readable**

---

## Notes

* Read-only
* May append duplicates if re-run
* Only supports Bichon v0.3.7 and earlier
* Bichon manages its own data storage and does not rely on external databases, Do not use network file systems (e.g. NFS, SMB) as the data directory, as network instability can easily lead to index or data corruption
---

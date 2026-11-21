use crate::aggregate::{Campaigns, Summary, UserDay};
use crate::catalog::Catalog;
use crate::keys::{Key, Keys};
use anyhow::{Context, Result, bail};
use parquet::basic::Compression;
use parquet::data_type::{ByteArray, ByteArrayType, DataType, Int32Type, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::{SerializedFileWriter, SerializedRowGroupWriter};
use parquet::schema::parser::parse_message_type;
use parquet::schema::types::ColumnPath;
use rayon::prelude::*;
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::NamedTempFile;

const USER: &str = "message user_daily {
    REQUIRED BINARY user_id (UTF8); REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions; REQUIRED INT64 events; REQUIRED INT64 revenue_cents;
    OPTIONAL BINARY top_category (UTF8);
}";
const CAMPAIGN: &str = "message campaign_daily {
    REQUIRED BINARY campaign_id (UTF8); REQUIRED BINARY channel (UTF8); REQUIRED INT32 day (DATE);
    REQUIRED INT64 sessions; REQUIRED INT64 purchasing_sessions; REQUIRED INT64 revenue_cents;
    REQUIRED INT64 assisted_sessions;
}";
const BATCH: usize = 64 * 1024;

fn file(dir: &Path, schema: &str) -> Result<(NamedTempFile, SerializedFileWriter<File>)> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = NamedTempFile::new_in(dir)?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_column_dictionary_enabled(ColumnPath::from("user_id"), false)
        .build();
    let writer = SerializedFileWriter::new(
        temp.reopen()?,
        Arc::new(parse_message_type(schema)?),
        Arc::new(props),
    )?;
    Ok((temp, writer))
}

fn column<T: DataType>(
    group: &mut SerializedRowGroupWriter<'_, File>,
    values: Vec<T::T>,
    levels: Option<&[i16]>,
) -> Result<()> {
    let mut column = group
        .next_column()?
        .context("missing column in output schema")?;
    column.typed::<T>().write_batch(&values, levels, None)?;
    column.close()?;
    Ok(())
}

fn user_batch(
    writer: &mut SerializedFileWriter<File>,
    rows: &[UserDay],
    keys: &Keys,
    categories: &[ByteArray],
) -> Result<()> {
    let mut group = writer.next_row_group()?;
    let mut last = Key::EMPTY;
    let mut bytes = ByteArray::default();
    let names = rows
        .iter()
        .map(|r| {
            if r.person != last {
                last = r.person;
                bytes = ByteArray::from(keys.bytes(last));
            }
            bytes.clone()
        })
        .collect();
    column::<ByteArrayType>(&mut group, names, None)?;
    column::<Int32Type>(&mut group, rows.iter().map(|r| r.day).collect(), None)?;
    column::<Int64Type>(&mut group, rows.iter().map(|r| r.sessions).collect(), None)?;
    column::<Int64Type>(&mut group, rows.iter().map(|r| r.events).collect(), None)?;
    column::<Int64Type>(&mut group, rows.iter().map(|r| r.revenue).collect(), None)?;
    let levels: Vec<_> = rows.iter().map(|r| i16::from(r.top.is_some())).collect();
    let values = rows
        .iter()
        .filter_map(|r| r.top.map(|i| categories[i as usize].clone()))
        .collect();
    column::<ByteArrayType>(&mut group, values, Some(&levels))?;
    group.close()?;
    Ok(())
}

const USER_PARTS: usize = 8;

fn user_file(
    dir: &Path,
    shards: Vec<Vec<UserDay>>,
    keys: &Keys,
    categories: &[ByteArray],
) -> Result<NamedTempFile> {
    let (temp, mut writer) = file(dir, USER)?;
    let mut batch = Vec::new();
    for row in shards.into_iter().flatten() {
        batch.push(row);
        if batch.len() == BATCH {
            user_batch(&mut writer, &batch, keys, categories)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        user_batch(&mut writer, &batch, keys, categories)?;
    }
    writer.close()?;
    Ok(temp)
}

fn campaign_file(
    dir: &Path,
    partials: Vec<Campaigns>,
    catalog: &Catalog,
) -> Result<(NamedTempFile, usize)> {
    let (temp, mut campaigns) = file(dir, CAMPAIGN)?;
    let mut rows = partials
        .into_par_iter()
        .reduce_with(Campaigns::merge)
        .map_or_else(Vec::new, Campaigns::into_rows);
    rows.sort_unstable_by_key(|r| r.0);
    for rows in rows.chunks(BATCH) {
        let mut group = campaigns.next_row_group()?;
        let names = rows
            .iter()
            .map(|((id, _), _)| ByteArray::from(catalog.campaigns[*id as usize].0.as_str()))
            .collect();
        let channels = rows
            .iter()
            .map(|((id, _), _)| ByteArray::from(catalog.campaigns[*id as usize].1.as_str()))
            .collect();
        column::<ByteArrayType>(&mut group, names, None)?;
        column::<ByteArrayType>(&mut group, channels, None)?;
        column::<Int32Type>(
            &mut group,
            rows.iter().map(|((_, day), _)| *day).collect(),
            None,
        )?;
        column::<Int64Type>(
            &mut group,
            rows.iter().map(|(_, r)| r.sessions).collect(),
            None,
        )?;
        column::<Int64Type>(
            &mut group,
            rows.iter().map(|(_, r)| r.purchasing).collect(),
            None,
        )?;
        column::<Int64Type>(
            &mut group,
            rows.iter().map(|(_, r)| r.revenue).collect(),
            None,
        )?;
        column::<Int64Type>(
            &mut group,
            rows.iter().map(|(_, r)| r.assists).collect(),
            None,
        )?;
        group.close()?;
    }
    campaigns.close()?;
    Ok((temp, rows.len()))
}

/// Serialize publishers. Hard links preserve the previous files without copying
/// their contents, so a failed rename can restore already-published partitions.
fn publish(root: &Path, files: Vec<(NamedTempFile, PathBuf)>) -> Result<()> {
    let directory = File::open(root)?;
    directory.lock().context("locking the output directory")?;
    let backups = tempfile::tempdir_in(root)?;
    let mut previous = Vec::with_capacity(files.len());
    for (i, (_, path)) in files.iter().enumerate() {
        let old = match fs::symlink_metadata(path) {
            Ok(stat) if stat.is_file() => {
                let backup = backups.path().join(i.to_string());
                fs::hard_link(path, &backup)?;
                Some(backup)
            }
            Ok(_) => bail!("expected a regular output file at {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        previous.push((path.clone(), old));
    }
    for (i, (temp, dest)) in files.into_iter().enumerate() {
        if let Err(error) = temp.persist(&dest) {
            for (path, old) in previous[..i].iter().rev() {
                let restored = match old {
                    Some(backup) => fs::rename(backup, path),
                    None => fs::remove_file(path),
                };
                if let Err(restore_error) = restored {
                    let saved = backups.keep();
                    bail!(
                        "publishing {}: {error}; restoring {}: {restore_error}; backups in {}",
                        dest.display(),
                        path.display(),
                        saved.display()
                    );
                }
            }
            return Err(error).with_context(|| format!("publishing {}", dest.display()));
        }
    }
    Ok(())
}

pub fn write(
    root: &Path,
    results: Vec<Summary>,
    keys: &Keys,
    catalog: &Catalog,
) -> Result<(usize, usize)> {
    let user_dir = root.join("user_daily");
    let camp_dir = root.join("campaign_daily");
    let mut users: [Vec<Vec<UserDay>>; USER_PARTS] = std::array::from_fn(|_| Vec::new());
    let mut partials = Vec::with_capacity(results.len());
    let mut nusers = 0;
    for (index, summary) in results.into_iter().enumerate() {
        nusers += summary.users.len();
        users[index % USER_PARTS].push(summary.users);
        partials.push(summary.campaigns);
    }
    let categories: Vec<_> = catalog
        .categories
        .iter()
        .map(|s| ByteArray::from(s.as_str()))
        .collect();
    let (user_files, campaign) = rayon::join(
        || {
            users
                .into_par_iter()
                .map(|shards| user_file(&user_dir, shards, keys, &categories))
                .collect::<Result<Vec<_>>>()
        },
        || campaign_file(&camp_dir, partials, catalog),
    );
    let user_files = user_files?;
    let (campaign, ncampaigns) = campaign?;
    // All footers are complete before publishing. Fixed part names also replace
    // output from a prior run, even when a later input is empty.
    let mut files: Vec<_> = user_files
        .into_iter()
        .enumerate()
        .map(|(i, temp)| (temp, user_dir.join(format!("part-{i:05}.parquet"))))
        .collect();
    files.push((campaign, camp_dir.join("part-00000.parquet")));
    publish(root, files)?;
    Ok((nusers, ncampaigns))
}

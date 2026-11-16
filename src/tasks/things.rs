//! Reading Things 3 (todo-spec §22). Things has no export; its data is one
//! SQLite database in its group container, which this reads — from a copy,
//! never the original, so a running Things is neither locked nor written to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::model::{
    ChangeContext, Task, TaskDates, TaskEvent, TaskLogEntry, TaskReminder, TaskStep,
    format_timestamp,
};
use super::recurrence::RecurrenceRule;
use crate::Result;

/// Where Things keeps its database, under the group container.
pub fn default_database() -> Option<PathBuf> {
    let root = PathBuf::from(
        shellexpand::tilde("~/Library/Group Containers/JLMPQHK86H.com.culturedcode.ThingsMac")
            .into_owned(),
    );
    std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .map(|entry| {
            entry
                .path()
                .join("Things Database.thingsdatabase")
                .join("main.sqlite")
        })
        .find(|path| path.is_file())
}

/// A Things to-do as Orb has it.
pub struct ThingsTask {
    /// Its id in Things.
    pub things_id: String,
    /// What the import makes of it, in one value: the same as long as nothing
    /// the import carries over has changed in Things.
    pub fingerprint: String,
    pub task: Task,
    pub reminders: Vec<TaskReminder>,
    /// Tag names: the task's own, its project's title and its area's.
    pub tag_names: Vec<String>,
    /// The messages its notes link to by `message://` URL, as link keys.
    pub links: Vec<String>,
    /// The Things rule that maps onto none of Orb's, by what it says.
    pub unmapped_rule: Option<String>,
}

/// What a Things database holds, as it would be imported.
#[derive(Default)]
pub struct ThingsData {
    pub tasks: Vec<ThingsTask>,
    /// Left out, and why: counts by reason.
    pub trashed: usize,
    /// The Things ids of the to-dos in its trash.
    pub trashed_ids: Vec<String>,
    pub projects: usize,
    pub headings: usize,
    /// Open instances of a repeating task, which the task itself stands for.
    pub repeat_instances: usize,
}

/// Read a Things database — from a copy, taken here with its `-wal` and
/// `-shm` — for an import into `account`.
pub fn read(database: &Path, account: &str, today: NaiveDate) -> Result<ThingsData> {
    let copy = std::env::temp_dir().join(format!("orb-things-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&copy)?;
    let read = (|| {
        let target = copy.join("main.sqlite");
        std::fs::copy(database, &target)?;
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", database.display()));
            if side.is_file() {
                std::fs::copy(&side, copy.join(format!("main.sqlite{suffix}")))?;
            }
        }
        let conn = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // The copy is ours: replaying the WAL into it touches nothing of Things'.
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .optional()?;
        read_connection(&conn, account, today)
    })();
    let _ = std::fs::remove_dir_all(&copy);
    read
}

/// The bits of a Things to-do the import reads.
struct Row {
    uuid: String,
    title: String,
    notes: Option<String>,
    kind: i64,
    status: i64,
    trashed: bool,
    start_date: Option<i64>,
    deadline: Option<i64>,
    reminder_time: Option<i64>,
    stop_date: Option<f64>,
    created: Option<f64>,
    modified: Option<f64>,
    index: i64,
    area: Option<String>,
    project: Option<String>,
    template: Option<String>,
    rule: Option<String>,
}

fn read_connection(conn: &Connection, account: &str, today: NaiveDate) -> Result<ThingsData> {
    let mut stmt = conn.prepare(
        "SELECT uuid, COALESCE(title, ''), notes, type, status, trashed, startDate, deadline,
                reminderTime, stopDate, creationDate, userModificationDate, \"index\", area,
                project, rt1_repeatingTemplate, CAST(rt1_recurrenceRule AS TEXT)
         FROM TMTask ORDER BY \"index\", uuid",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            Ok(Row {
                uuid: r.get(0)?,
                title: r.get(1)?,
                notes: r.get(2)?,
                kind: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                status: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                trashed: r.get::<_, Option<i64>>(5)?.unwrap_or(0) != 0,
                start_date: r.get(6)?,
                deadline: r.get(7)?,
                reminder_time: r.get(8)?,
                stop_date: r.get(9)?,
                created: r.get(10)?,
                modified: r.get(11)?,
                index: r.get::<_, Option<i64>>(12)?.unwrap_or(0),
                area: r.get(13)?,
                project: r.get(14)?,
                template: r.get(15)?,
                rule: r.get(16)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let areas: HashMap<String, String> = names(conn, "SELECT uuid, title FROM TMArea")?;
    let projects: HashMap<&str, &Row> = rows
        .iter()
        .filter(|r| r.kind == 1)
        .map(|r| (r.uuid.as_str(), r))
        .collect();
    let tag_names: HashMap<String, String> = names(conn, "SELECT uuid, title FROM TMTag")?;
    let mut tags_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut tag_stmt = conn.prepare("SELECT tasks, tags FROM TMTaskTag")?;
    for pair in tag_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (task, tag) = pair?;
        if let Some(name) = tag_names.get(&tag) {
            tags_of.entry(task).or_default().push(name.clone());
        }
    }
    let mut steps_of: HashMap<String, Vec<(i64, TaskStep)>> = HashMap::new();
    let mut step_stmt = conn.prepare(
        "SELECT uuid, task, COALESCE(title, ''), status, \"index\" FROM TMChecklistItem",
    )?;
    for step in step_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        ))
    })? {
        let (uuid, task, title, status, index) = step?;
        steps_of.entry(task).or_default().push((
            index,
            TaskStep {
                id: derived_id("things-step", &uuid),
                title,
                done: status == 3,
            },
        ));
    }
    // A repeating task's open instance: the task itself stands for it, with
    // the instance's date as its own.
    let mut open_instance_date: HashMap<&str, i64> = HashMap::new();
    for row in &rows {
        if let (Some(template), 0, false, Some(date)) =
            (&row.template, row.status, row.trashed, row.start_date)
        {
            open_instance_date.entry(template.as_str()).or_insert(date);
        }
    }

    let mut data = ThingsData::default();
    for row in &rows {
        match row.kind {
            1 => {
                data.projects += 1;
                continue;
            }
            2 => {
                data.headings += 1;
                continue;
            }
            _ => {}
        }
        if row.trashed {
            data.trashed += 1;
            data.trashed_ids.push(row.uuid.clone());
            continue;
        }
        if row.template.is_some() && row.status == 0 {
            data.repeat_instances += 1;
            continue;
        }

        let created = instant(row.created).unwrap_or_else(Utc::now);
        let ctx = ChangeContext::new(created, today).with_device_id(Some("things"));
        let mut task = Task::bare(
            derived_id("things", &row.uuid),
            row.title.clone(),
            account.to_string(),
            row.index as f64,
            format_timestamp(created),
        );
        task.body_text = row.notes.clone().filter(|n| !n.trim().is_empty());
        task.updated_at = format_timestamp(instant(row.modified).unwrap_or(created));

        let mut start = row.start_date.and_then(packed_date);
        let mut unmapped_rule = None;
        if let Some(rule) = row.rule.as_deref().filter(|_| row.template.is_none()) {
            if let Some(&date) = open_instance_date.get(row.uuid.as_str()) {
                start = packed_date(date);
            }
            match recurrence(rule) {
                Some((rrule, from_completion)) => {
                    task.recurrence = Some(rrule);
                    task.recurrence_from_completion = from_completion;
                    if start.is_none() {
                        start = plist_real(rule, "ia")
                            .and_then(|at| instant(Some(at)))
                            .map(|at| at.with_timezone(&Local).date_naive());
                    }
                }
                None => {
                    unmapped_rule = Some(describe_rule(rule));
                    start = None;
                }
            }
        }
        let due = row.deadline.and_then(packed_date);
        task.set_dates(TaskDates::new(start, due));

        let mut steps = steps_of.remove(&row.uuid).unwrap_or_default();
        steps.sort_by_key(|(index, step)| (*index, step.id.clone()));
        task.steps = steps.into_iter().map(|(_, step)| step).collect();

        // Ids derived rather than random, so importing again brings no
        // second copy of a line the log already has.
        let entry = |event: TaskEvent, ctx: &ChangeContext<'_>, kind: &str| {
            let mut entry = TaskLogEntry::new(event, ctx);
            entry.entry_uuid = derived_id(kind, &row.uuid);
            entry
        };
        task.log = vec![entry(
            note("Created in Things".to_string()),
            &ctx,
            "things-created",
        )];
        if let Some(rule) = &unmapped_rule {
            task.log.push(entry(
                note(format!(
                    "Repeated in Things ({rule}), which Orb cannot express; set it again \
                     with set-recurrence"
                )),
                &ctx,
                "things-rule",
            ));
        }
        if row.status == 3 || row.status == 2 {
            let stopped = instant(row.stop_date).unwrap_or(created);
            task.is_completed = true;
            task.completed_at = Some(format_timestamp(stopped));
            if row.status == 2 {
                let at = ChangeContext::new(stopped, today).with_device_id(Some("things"));
                task.log.push(entry(
                    note("Cancelled in Things".to_string()),
                    &at,
                    "things-cancelled",
                ));
            }
        }

        let mut reminders = Vec::new();
        if let (Some(day), Some(time)) = (start, row.reminder_time.and_then(packed_time))
            && let Some(local) = Local.from_local_datetime(&day.and_time(time)).earliest()
        {
            let at = local.with_timezone(&Utc);
            reminders.push(TaskReminder::new(0, format_timestamp(at), at.timestamp()));
        }

        let mut tag_names = tags_of.remove(&row.uuid).unwrap_or_default();
        let project = row.project.as_deref().and_then(|p| projects.get(p));
        if let Some(project) = project {
            tag_names.push(project.title.clone());
        }
        let area = row
            .area
            .as_ref()
            .or_else(|| project.and_then(|p| p.area.as_ref()));
        if let Some(name) = area.and_then(|a| areas.get(a)) {
            tag_names.push(name.clone());
        }
        tag_names.retain(|name| !name.trim().is_empty());
        tag_names.sort();
        tag_names.dedup();

        let links = task
            .body_text
            .as_deref()
            .map(super::message_url_ids)
            .unwrap_or_default();
        data.tasks.push(ThingsTask {
            things_id: row.uuid.clone(),
            fingerprint: fingerprint(
                &task,
                &reminders,
                &tag_names,
                &links,
                unmapped_rule.as_deref(),
            ),
            task,
            reminders,
            tag_names,
            links,
            unmapped_rule,
        });
    }
    Ok(data)
}

fn names(conn: &Connection, sql: &str) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare(sql)?;
    let pairs = stmt
        .query_map(params![], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(pairs)
}

/// The log entry that records an import of a to-do: what it was in Things
/// and the revision the import leaves the task at.
pub fn import_mark(item: &ThingsTask, revision: i64, ctx: &ChangeContext<'_>) -> TaskLogEntry {
    let mut entry = TaskLogEntry::new(
        TaskEvent::Imported {
            note: if revision > 1 {
                "Updated from Things".to_string()
            } else {
                format!("Imported from Things ({})", item.things_id)
            },
            source: Some(source(&item.things_id)),
            fingerprint: Some(item.fingerprint.clone()),
            revision: Some(revision),
        },
        ctx,
    );
    entry.entry_uuid = derived_id(
        "things-mark",
        &format!("{}:{}:{revision}", item.things_id, item.fingerprint),
    );
    entry
}

/// What the last import of a to-do made of it, and the revision it left the
/// task at; a task edited since is past that revision. A task from an
/// import that kept no mark was left at revision 1, with nothing known of
/// what it was.
pub fn last_import(task: &Task, things_id: &str) -> Option<(Option<String>, i64)> {
    let source = source(things_id);
    let legacy = format!("Imported from Things ({things_id})");
    let mut found = None;
    for entry in &task.log {
        if let TaskEvent::Imported {
            note,
            source: from,
            fingerprint,
            revision,
        } = &entry.event
        {
            match (from, revision) {
                (Some(from), Some(revision)) if *from == source => {
                    if found.as_ref().is_none_or(|(_, at)| revision >= at) {
                        found = Some((fingerprint.clone(), *revision));
                    }
                }
                (None, None) if *note == legacy && found.is_none() => found = Some((None, 1)),
                _ => {}
            }
        }
    }
    found
}

fn source(things_id: &str) -> String {
    format!("things:{things_id}")
}

/// Everything the import carries over to a task, hashed.
fn fingerprint(
    task: &Task,
    reminders: &[TaskReminder],
    tag_names: &[String],
    links: &[String],
    unmapped_rule: Option<&str>,
) -> String {
    let steps: Vec<(&str, bool)> = task
        .steps
        .iter()
        .map(|step| (step.title.as_str(), step.done))
        .collect();
    let reminders: Vec<i64> = reminders.iter().map(|r| r.remind_at_unix).collect();
    let mut mapped = serde_json::json!([
        task.subject,
        task.body_text,
        task.postpone_date,
        task.due_date,
        task.recurrence,
        task.recurrence_from_completion,
        task.is_completed,
        task.completed_at,
        steps,
        reminders,
        tag_names,
        unmapped_rule,
    ]);
    // Only where there are any, so that what an import before links were
    // carried over left behind still matches.
    if !links.is_empty()
        && let Some(fields) = mapped.as_array_mut()
    {
        fields.push(serde_json::json!(links));
    }
    let digest = Sha256::digest(mapped.to_string().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// A line in the log about the import.
fn note(note: String) -> TaskEvent {
    TaskEvent::Imported {
        note,
        source: None,
        fingerprint: None,
        revision: None,
    }
}

/// An Orb id for a Things id, the same every time, so a second import finds
/// what the first wrote. Shaped as a UUID, version 5 style.
pub fn derived_id(namespace: &str, things_uuid: &str) -> String {
    let hash = Sha256::digest(format!("{namespace}:{things_uuid}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&hash[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// A Things date: `year << 16 | month << 12 | day << 7`.
///
/// Things keeps placeholder dates in the year 4000 and later on repeating
/// templates; none of them is a date anybody meant.
pub fn packed_date(packed: i64) -> Option<NaiveDate> {
    if packed <= 0 || packed >> 16 >= PLACEHOLDER_YEAR {
        return None;
    }
    NaiveDate::from_ymd_opt(
        (packed >> 16) as i32,
        ((packed >> 12) & 0xf) as u32,
        ((packed >> 7) & 0x1f) as u32,
    )
}

const PLACEHOLDER_YEAR: i64 = 4000;

/// A Things time of day: `hour << 26 | minute << 20`.
pub fn packed_time(packed: i64) -> Option<chrono::NaiveTime> {
    chrono::NaiveTime::from_hms_opt(
        ((packed >> 26) & 0x1f) as u32,
        ((packed >> 20) & 0x3f) as u32,
        0,
    )
}

/// A Things instant: seconds since the Unix epoch, as a float.
fn instant(seconds: Option<f64>) -> Option<DateTime<Utc>> {
    let seconds = seconds?;
    Utc.timestamp_opt(seconds as i64, 0).single()
}

/// A Things repetition (an XML property list) as an `RRULE` Orb accepts, and
/// whether it advances from the completion; `None` where it does not map.
pub fn recurrence(plist: &str) -> Option<(String, bool)> {
    let unit = plist_integer(plist, "fu")?;
    let every = plist_integer(plist, "fa").unwrap_or(1).max(1);
    let offsets = plist_offsets(plist);
    let mut parts = vec![format!(
        "FREQ={}",
        match unit {
            16 => "DAILY",
            256 => "WEEKLY",
            8 => "MONTHLY",
            4 => "YEARLY",
            _ => return None,
        }
    )];
    if every > 1 {
        parts.push(format!("INTERVAL={every}"));
    }
    match unit {
        256 => {
            const DAYS: [&str; 7] = ["SU", "MO", "TU", "WE", "TH", "FR", "SA"];
            let days: Vec<&str> = offsets
                .iter()
                .filter_map(|o| o.get("wd").and_then(|wd| DAYS.get(*wd as usize).copied()))
                .collect();
            if !days.is_empty() {
                parts.push(format!("BYDAY={}", days.join(",")));
            }
        }
        8 => {
            // Day of the month, from 0; -1 is the last. A weekday of the month
            // ("the second Monday") Orb cannot say.
            if offsets.iter().any(|o| o.contains_key("wd")) {
                return None;
            }
            let days: Vec<String> = offsets
                .iter()
                .filter_map(|o| o.get("dy"))
                .map(|dy| {
                    if *dy < 0 {
                        dy.to_string()
                    } else {
                        (dy + 1).to_string()
                    }
                })
                .collect();
            if !days.is_empty() {
                parts.push(format!("BYMONTHDAY={}", days.join(",")));
            }
        }
        4 if offsets.iter().any(|o| o.contains_key("wd")) => return None,
        _ => {}
    }
    let count = plist_integer(plist, "rc").unwrap_or(0);
    let end = plist_real(plist, "ed").filter(|ed| *ed < 60_000_000_000.0);
    if count > 0 {
        parts.push(format!("COUNT={count}"));
    } else if let Some(end) = end.and_then(|e| instant(Some(e))) {
        parts.push(format!("UNTIL={}", end.format("%Y%m%dT%H%M%SZ")));
    }
    let rule = parts.join(";");
    RecurrenceRule::parse(&rule).ok()?;
    Some((rule, plist_integer(plist, "tp") == Some(1)))
}

/// A rule Orb cannot express, in a few words for the log.
fn describe_rule(plist: &str) -> String {
    let every = plist_integer(plist, "fa").unwrap_or(1);
    let unit = match plist_integer(plist, "fu") {
        Some(16) => "day",
        Some(256) => "week",
        Some(8) => "month",
        Some(4) => "year",
        _ => "period",
    };
    format!("every {every} {unit}, on a pattern")
}

/// `<key>name</key> <integer>n</integer>` at the top of the dictionary.
fn plist_integer(plist: &str, key: &str) -> Option<i64> {
    let after = plist.split(&format!("<key>{key}</key>")).nth(1)?;
    let value = after.trim_start().strip_prefix("<integer>")?;
    value.split("</integer>").next()?.trim().parse().ok()
}

fn plist_real(plist: &str, key: &str) -> Option<f64> {
    let after = plist.split(&format!("<key>{key}</key>")).nth(1)?;
    let value = after
        .trim_start()
        .strip_prefix("<real>")
        .or_else(|| after.trim_start().strip_prefix("<integer>"))?;
    value.split(['<']).next()?.trim().parse().ok()
}

/// The offsets array, `of`: one small dictionary of integers per entry.
fn plist_offsets(plist: &str) -> Vec<HashMap<String, i64>> {
    let Some(after) = plist.split("<key>of</key>").nth(1) else {
        return Vec::new();
    };
    let Some(array) = after.split("</array>").next() else {
        return Vec::new();
    };
    array
        .split("<dict>")
        .skip(1)
        .map(|dict| {
            let mut map = HashMap::new();
            for pair in dict.split("<key>").skip(1) {
                let Some((key, rest)) = pair.split_once("</key>") else {
                    continue;
                };
                if let Some(value) = rest
                    .trim_start()
                    .strip_prefix("<integer>")
                    .and_then(|v| v.split("</integer>").next())
                    .and_then(|v| v.trim().parse().ok())
                {
                    map.insert(key.trim().to_string(), value);
                }
            }
            map
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn the_last_import_is_read_from_the_log_and_an_unmarked_one_was_revision_1() {
        let ctx = ChangeContext::new(Utc::now(), NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let mut task = Task::bare(
            derived_id("things", "a"),
            "Buy seeds".to_string(),
            "Tasks".to_string(),
            1.0,
            format_timestamp(Utc::now()),
        );
        assert_eq!(last_import(&task, "a"), None);
        task.log.push(TaskLogEntry::new(
            note("Imported from Things (a)".to_string()),
            &ctx,
        ));
        assert_eq!(last_import(&task, "a"), Some((None, 1)));
        let bare = task.clone();
        let item = |fingerprint: &str| ThingsTask {
            things_id: "a".to_string(),
            fingerprint: fingerprint.to_string(),
            task: bare.clone(),
            reminders: Vec::new(),
            tag_names: Vec::new(),
            links: Vec::new(),
            unmapped_rule: None,
        };
        task.log.push(import_mark(&item("first"), 2, &ctx));
        task.log.push(import_mark(&item("second"), 5, &ctx));
        assert_eq!(
            last_import(&task, "a"),
            Some((Some("second".to_string()), 5))
        );
        assert_eq!(last_import(&task, "b"), None);
    }

    #[test]
    fn packed_dates_decode_at_the_edges_of_months_and_years() {
        let pack = |y: i64, m: i64, d: i64| (y << 16) | (m << 12) | (d << 7);
        for (y, m, d) in [(2025, 12, 31), (2026, 1, 1), (2024, 2, 29), (2025, 5, 19)] {
            assert_eq!(
                packed_date(pack(y, m, d)),
                NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)
            );
        }
        assert_eq!(packed_date(0), None);
        assert_eq!(packed_date(pack(4001, 1, 29)), None);
        assert_eq!(
            packed_time((16 << 26) | (10 << 20)),
            chrono::NaiveTime::from_hms_opt(16, 10, 0)
        );
    }

    fn rule(unit: i64, every: i64, offsets: &str, tp: i64) -> String {
        format!(
            "<plist version=\"1.0\"><dict><key>ed</key><real>64092211200</real>\
             <key>fa</key><integer>{every}</integer><key>fu</key><integer>{unit}</integer>\
             <key>ia</key><real>1756684800</real><key>of</key><array>{offsets}</array>\
             <key>rc</key><integer>0</integer><key>tp</key><integer>{tp}</integer></dict></plist>"
        )
    }

    #[test]
    fn things_repetitions_map_onto_rrules_or_say_they_do_not() {
        assert_eq!(
            recurrence(&rule(
                256,
                1,
                "<dict><key>wd</key><integer>1</integer></dict>",
                0
            )),
            Some(("FREQ=WEEKLY;BYDAY=MO".to_string(), false))
        );
        assert_eq!(
            recurrence(&rule(16, 3, "", 1)),
            Some(("FREQ=DAILY;INTERVAL=3".to_string(), true))
        );
        assert_eq!(
            recurrence(&rule(
                8,
                1,
                "<dict><key>dy</key><integer>-1</integer></dict>",
                0
            )),
            Some(("FREQ=MONTHLY;BYMONTHDAY=-1".to_string(), false))
        );
        assert_eq!(
            recurrence(&rule(
                8,
                1,
                "<dict><key>dy</key><integer>14</integer></dict>",
                0
            )),
            Some(("FREQ=MONTHLY;BYMONTHDAY=15".to_string(), false))
        );
        // The second Monday of the month: not in Orb's subset.
        assert_eq!(
            recurrence(&rule(
                8,
                1,
                "<dict><key>wd</key><integer>1</integer><key>wdo</key><integer>2</integer></dict>",
                0
            )),
            None
        );
    }

    #[test]
    fn a_derived_id_is_stable_and_shaped_as_a_uuid() {
        let id = derived_id("things", "ABC");
        assert_eq!(id, derived_id("things", "ABC"));
        assert_ne!(id, derived_id("things", "ABD"));
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "5");
    }

    /// A Things database with the columns the import reads, in Things' own
    /// table and column names.
    pub(crate) fn fixture(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE TMTask (uuid TEXT PRIMARY KEY, title TEXT, notes TEXT, type INTEGER,
                status INTEGER, trashed INTEGER, startDate INTEGER, deadline INTEGER,
                reminderTime INTEGER, stopDate REAL, creationDate REAL, userModificationDate REAL,
                \"index\" INTEGER, area TEXT, project TEXT, rt1_repeatingTemplate TEXT,
                rt1_recurrenceRule BLOB);
             CREATE TABLE TMChecklistItem (uuid TEXT PRIMARY KEY, title TEXT, status INTEGER,
                \"index\" INTEGER, task TEXT);
             CREATE TABLE TMTag (uuid TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE TMTaskTag (tasks TEXT NOT NULL, tags TEXT NOT NULL);
             CREATE TABLE TMArea (uuid TEXT PRIMARY KEY, title TEXT);",
        )
        .unwrap();
        let d = |y: i64, m: i64, day: i64| (y << 16) | (m << 12) | (day << 7);
        let weekly = rule(256, 1, "<dict><key>wd</key><integer>1</integer></dict>", 0);
        conn.execute("INSERT INTO TMArea VALUES ('area', 'Home')", [])
            .unwrap();
        conn.execute("INSERT INTO TMTag VALUES ('tag', 'errand')", [])
            .unwrap();
        let task = |uuid: &str,
                    title: &str,
                    kind: i64,
                    status: i64,
                    trashed: i64,
                    start: Option<i64>,
                    deadline: Option<i64>,
                    reminder: Option<i64>,
                    project: Option<&str>,
                    template: Option<&str>,
                    rule: Option<&str>| {
            conn.execute(
                "INSERT INTO TMTask VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, 1760000000.0,
                    1750000000.0, 1755000000.0, 1, NULL, ?9, ?10, ?11)",
                params![
                    uuid, title, kind, status, trashed, start, deadline, reminder, project,
                    template, rule
                ],
            )
            .unwrap();
        };
        task(
            "project", "Garden", 1, 0, 0, None, None, None, None, None, None,
        );
        conn.execute("UPDATE TMTask SET area = 'area' WHERE uuid = 'project'", [])
            .unwrap();
        task(
            "heading",
            "Spring",
            2,
            0,
            0,
            None,
            None,
            None,
            Some("project"),
            None,
            None,
        );
        task(
            "open",
            "Buy seeds",
            0,
            0,
            0,
            Some(d(2025, 12, 31)),
            Some(d(2026, 1, 2)),
            Some((9 << 26) | (30 << 20)),
            Some("project"),
            None,
            None,
        );
        task(
            "done",
            "Mow the lawn",
            0,
            3,
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        task(
            "cancelled",
            "Hire a gardener",
            0,
            2,
            0,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        task(
            "trashed", "Old idea", 0, 0, 1, None, None, None, None, None, None,
        );
        task(
            "repeat",
            "Water the plants",
            0,
            0,
            0,
            None,
            None,
            None,
            None,
            None,
            Some(&weekly),
        );
        task(
            "instance-open",
            "Water the plants",
            0,
            0,
            0,
            Some(d(2026, 3, 2)),
            None,
            None,
            None,
            Some("repeat"),
            None,
        );
        task(
            "instance-done",
            "Water the plants",
            0,
            3,
            0,
            Some(d(2026, 2, 23)),
            None,
            None,
            None,
            Some("repeat"),
            None,
        );
        conn.execute("INSERT INTO TMTaskTag VALUES ('open', 'tag')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO TMChecklistItem VALUES ('c2', 'Rake', 0, 2, 'open')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO TMChecklistItem VALUES ('c1', 'Dig', 3, 1, 'open')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn a_things_database_maps_onto_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("main.sqlite");
        fixture(&db);
        let data = read(&db, "Work", NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()).unwrap();
        assert_eq!(
            (
                data.projects,
                data.headings,
                data.trashed,
                data.repeat_instances
            ),
            (1, 1, 1, 1)
        );
        let by = |title: &str| data.tasks.iter().find(|t| t.task.subject == title).unwrap();
        let titles: Vec<&str> = data.tasks.iter().map(|t| t.task.subject.as_str()).collect();
        assert_eq!(titles.len(), 5, "{titles:?}");

        let open = by("Buy seeds");
        assert_eq!(open.task.postpone_date.as_deref(), Some("2025-12-31"));
        assert_eq!(open.task.due_date.as_deref(), Some("2026-01-02"));
        assert_eq!(open.tag_names, ["Garden", "Home", "errand"]);
        let steps: Vec<(&str, bool)> = open
            .task
            .steps
            .iter()
            .map(|s| (s.title.as_str(), s.done))
            .collect();
        assert_eq!(steps, [("Dig", true), ("Rake", false)]);
        assert_eq!(open.reminders.len(), 1);
        assert_eq!(open.task.task_uuid, derived_id("things", "open"));

        assert!(by("Mow the lawn").task.is_completed);
        let cancelled = by("Hire a gardener");
        assert!(cancelled.task.is_completed);
        assert!(
            cancelled
                .task
                .log
                .iter()
                .any(|e| e.rendered_text().contains("Cancelled in Things"))
        );

        let repeat = data
            .tasks
            .iter()
            .find(|t| t.task.task_uuid == derived_id("things", "repeat"))
            .unwrap();
        assert_eq!(
            repeat.task.recurrence.as_deref(),
            Some("FREQ=WEEKLY;BYDAY=MO")
        );
        assert_eq!(repeat.task.postpone_date.as_deref(), Some("2026-03-02"));
        // The finished instance comes over as history.
        assert!(
            data.tasks
                .iter()
                .any(|t| t.task.task_uuid == derived_id("things", "instance-done"))
        );
    }
}

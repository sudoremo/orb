/**
 * The console: the log as it is written, and the commands the shell ran
 * lately with what went wrong. Read every two seconds; a window, not a
 * stream, because what it shows is a ring the core keeps anyway.
 */

import { escape } from "./format";
import { invoke } from "@tauri-apps/api/core";
import { bindTabShortcuts } from "./tabs";
import "./styles.css";
import { followAppearance } from "./windowlook";

void followAppearance();

interface JobRecord {
  label: string;
  started_unix: number;
  took_ms: number;
  error: string | null;
}

const el = {
  jobs: document.getElementById("jobs") as HTMLTableElement,
  log: document.getElementById("log") as HTMLPreElement,
  path: document.getElementById("log-path") as HTMLSpanElement,
};


/** Whether the log is scrolled to its end, which is where new lines go. */
function atEnd(): boolean {
  return el.log.scrollHeight - el.log.scrollTop - el.log.clientHeight < 8;
}

async function refresh(): Promise<void> {
  const jobs = await invoke<JobRecord[]>("recent_jobs").catch(() => [] as JobRecord[]);
  el.jobs.innerHTML = jobs
    .slice(0, 40)
    .map(
      (job) =>
        `<tr class="${job.error ? "failed" : ""}"><td class="dim">${escape(
          new Date(job.started_unix * 1000).toTimeString().slice(0, 8),
        )}</td><td>${escape(job.label)}</td><td class="dim">${job.took_ms} ms</td><td>${escape(
          job.error ?? "",
        )}</td></tr>`,
    )
    .join("");

  const [lines, path] = await invoke<[string[], string | null]>("log_recent", {
    lines: 500,
  }).catch(() => [[], null] as [string[], string | null]);
  const follow = atEnd();
  el.path.textContent = path ? path : "not written to a file; set log_file in config.yml";
  el.log.textContent = lines.join("\n");
  if (follow) el.log.scrollTop = el.log.scrollHeight;
}

bindTabShortcuts();
void refresh();
setInterval(() => void refresh(), 2000);

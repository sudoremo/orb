/**
 * Files a person chooses, through a dialog the shell opens.
 *
 * The shell notes what was picked, so the paths handed back to it are ones a
 * person chose and it reads them without asking again; a path from anywhere
 * else is asked about in a dialog of the system's. The page therefore never
 * opens a file dialog itself.
 */

import { invoke } from "@tauri-apps/api/core";

/** Pick files, a file or a folder; nothing when the dialog was cancelled. */
export async function pickFiles(options: {
  kind: "files" | "file" | "folder";
  title: string;
  defaultPath?: string;
}): Promise<string[]> {
  const picked = await invoke<string[] | null>("pick_files", {
    kind: options.kind,
    title: options.title,
    defaultPath: options.defaultPath ?? null,
  });
  return picked ?? [];
}

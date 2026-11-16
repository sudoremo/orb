/**
 * Orb's own icons, from the guide in `docs/mockups/main-window.html`: Lucide
 * shapes on a 24-unit grid, drawn whole in the text colour, with at most one
 * line — the detail that names the thing — in `.duo`. Only places (the
 * sidebar, the detail pane) colour that line; everywhere else it takes the
 * outline's colour.
 */
const OUTLINE = {
  sun: '<circle cx="12" cy="12" r="4"/>',
  cal: '<rect x="3" y="4" width="18" height="18" rx="2"/><path d="M3 10h18"/>',
  cloud: '<path d="M17.5 19H9a7 7 0 1 1 6.71-9h1.79a4.5 4.5 0 1 1 0 9Z"/>',
  done: '<circle cx="12" cy="12" r="10"/>',
  book: '<path d="M2 4h6a4 4 0 0 1 4 4v13a3 3 0 0 0-3-3H2z"/><path d="M22 4h-6a4 4 0 0 0-4 4v13a3 3 0 0 1 3-3h7z"/>',
  inbox: '<path d="M5.45 5.11 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.45-6.89A2 2 0 0 0 16.76 4H7.24a2 2 0 0 0-1.79 1.11z"/>',
  draft: '<path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  send: '<path d="m22 2-7 20-4-9-9-4Z"/>',
  outbox: '<path d="M22 13h-6l-2 3h-4l-2-3H2"/><path d="M2 13v5a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-5"/>',
  archive: '<rect x="2" y="3" width="20" height="5" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/>',
  junk: '<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z"/>',
  trash: '<path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/>',
  clip: '<path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48"/>',
  flag: '<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"/><path d="M4 22v-7"/>',
  search: '<circle cx="11" cy="11" r="7"/>',
  chev: '<path d="m6 9 6 6 6-6"/>',
  bell: '<path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9"/>',
  mail: '<rect x="2" y="4" width="20" height="16" rx="2"/>',
  task: '<rect x="3" y="3" width="18" height="18" rx="4"/>',
  tag: '<path d="M12 2H2v10l9.29 9.29a1 1 0 0 0 1.41 0l8.59-8.59a1 1 0 0 0 0-1.41Z"/>',
  sort: '<path d="m3 16 4 4 4-4M7 20V4M21 8l-4-4-4 4M17 4v16"/>',
  filter: '<path d="M3 6h18M7 12h10M10 18h4"/>',
  user: '<circle cx="12" cy="8" r="4"/><path d="M4 21a8 8 0 0 1 16 0"/>',
  text: '<path d="M4 7V5h16v2M12 5v14M9 19h6"/>',
  reply: '<path d="M9 17 4 12l5-5"/><path d="M20 18v-2a4 4 0 0 0-4-4H4"/>',
  alert: '<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><path d="M12 9v4M12 17h.01"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  arrow: '<path d="M12 5v14M19 12l-7 7-7-7"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  rows: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 12h18"/>',
  cols: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M12 3v18"/>',
  mixed: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M15 3v18M7 8h4M7 12h4M7 16h4"/>',
  sidebar: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M9 3v18"/>',
  chat: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
  hash: '<path d="M4 9h16M4 15h16M10 3 8 21M16 3l-2 18"/>',
  smile: '<circle cx="12" cy="12" r="10"/><path d="M8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01"/>',
  repeat: '<path d="m17 2 4 4-4 4"/><path d="M3 11v-1a4 4 0 0 1 4-4h14"/><path d="m7 22-4-4 4-4"/><path d="M21 13v1a4 4 0 0 1-4 4H3"/>',
  tpl: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a1 1 0 0 0 1 1h5"/>',
  braces: '<path d="M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5a2 2 0 0 0 2 2h1M16 21h1a2 2 0 0 0 2-2v-5a2 2 0 0 1 2-2 2 2 0 0 1-2-2V5a2 2 0 0 0-2-2h-1"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  mute: '<path d="M8.7 3A6 6 0 0 1 18 8a21.3 21.3 0 0 0 .6 5"/><path d="M17 17H3s3-2 3-9a4.67 4.67 0 0 1 .3-1.7"/><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/>',
  // A message with others behind it: a conversation.
  thread: '<rect x="2" y="8" width="16" height="12" rx="2"/><path d="M6 4h14a2 2 0 0 1 2 2v10"/>',
} as const;

/** The accent line of each icon that has a detail to colour. */
const DETAIL: Partial<Record<IconName, string>> = {
  sun: '<path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/>',
  cal: '<path d="M16 2v4M8 2v4"/>',
  done: '<path d="m9 12 2 2 4-4"/>',
  mute: '<path d="m2 2 20 20"/>',
  inbox: '<path d="M22 12h-6l-2 3h-4l-2-3H2"/>',
  draft: '<path d="M12 20h9"/>',
  send: '<path d="M22 2 11 13"/>',
  outbox: '<path d="M12 3v9M8 7l4-4 4 4"/>',
  archive: '<path d="M10 12h4"/>',
  junk: '<path d="M9.5 9.5l5 5M14.5 9.5l-5 5"/>',
  trash: '<path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>',
  mail: '<path d="m22 7-10 5L2 7"/>',
  task: '<path d="m8.5 12 2.5 2.5 4.5-5"/>',
  bell: '<path d="M10.3 21a1.94 1.94 0 0 0 3.4 0"/>',
  tag: '<circle cx="7" cy="7" r="1.5"/>',
  search: '<path d="m20 20-3.5-3.5"/>',
  chat: '<path d="M8 9h8M8 13h5"/>',
  thread: '<path d="m18 10-8 5-8-5"/>',
};

export type IconName = keyof typeof OUTLINE;

/** An icon as inline SVG, sized and coloured by CSS (`.ic`). */
export function svgIcon(name: IconName, cls = ""): string {
  const detail = DETAIL[name];
  return `<svg class="ic${cls ? ` ${cls}` : ""}" viewBox="0 0 24 24" aria-hidden="true">${
    detail ? `<g class="duo">${detail}</g>` : ""
  }${OUTLINE[name]}</svg>`;
}

/** The icon of a folder with this role, or the plain folder. */
export function roleIcon(role: string | null | undefined): IconName {
  switch (role) {
    case "inbox": return "inbox";
    case "drafts": return "draft";
    case "sent": return "send";
    case "archive": return "archive";
    case "junk": return "junk";
    case "trash": return "trash";
    default: return "folder";
  }
}

import { Draft } from "./draft";
import { currentFolder } from "./actions";
import { call, setStatus } from "./bridge";
import { MessageHeader } from "./model";
import { state } from "./state";

/** A draft as the core builds it. */
export interface DraftTemplate {
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  body: string;
  in_reply_to: string | null;
  forwards?: string | null;
  references: string | null;
  attachments: string[];
  identity: string;
  /** Opened without its Bcc or attachments, so it must not replace the saved copy. */
  incomplete?: boolean;
  addressed_to?: string[];
  delivered_to?: string[];
}
/** The same draft in the shape the compose window is opened with. */
export function fromTemplate(t: DraftTemplate): Draft & { attachments: string[]; identity: string } {
  return {
    to: t.to,
    cc: t.cc,
    bcc: t.bcc,
    subject: t.subject,
    body: t.body,
    inReplyTo: t.in_reply_to ?? undefined,
    references: t.references ?? undefined,
    forwards: t.forwards ?? undefined,
    attachments: t.attachments,
    identity: t.identity,
    addressedTo: t.addressed_to ?? [],
    deliveredTo: t.delivered_to ?? [],
  };
}
/** Whether the folder on screen is where this account keeps its drafts. */
export function viewingDrafts(): boolean {
  const folder = currentFolder();
  if (!folder?.folder_name) return false;
  const account = state.accounts.find((a) => a.name === folder.account_name);
  return account?.drafts_folder === folder.folder_name;
}
/**
 * Reopen a saved draft in a compose window.
 *
 * A draft is mail that is not finished, so opening one continues it rather than
 * showing it. The UID travels with it: saving again replaces the copy on the
 * server instead of leaving the half-written version behind next to the new one.
 */
export async function editDraft(msg: MessageHeader, account: string): Promise<void> {
  const draft = await call<DraftTemplate>("edit_draft", { id: msg.id });
  if (draft.incomplete) {
    // Saved as a new draft beside the old one, which keeps what could not
    // be read here.
    await openCompose(account, fromTemplate(draft));
    setStatus("the server could not be reached: the draft opens without its Bcc or attachments, and saves as a copy");
    return;
  }
  await openCompose(account, fromTemplate(draft), msg.uid);
}
/**
 * Open a draft in a window of its own.
 *
 * Composition lives in a separate window rather than in this one, so several
 * drafts can be open at once and macOS can group them into tabs — the tab bar,
 * dragging one out, and moving between them all come from the system rather
 * than from a tab strip drawn here.
 *
 * The draft is left with the shell for the window to collect, because the
 * window has to exist before it can be spoken to, and a reply arrives already
 * carrying its quoted body and threading headers.
 *
 * `account` is the one the message goes out from: for a reply or a forward
 * the account the original arrived at, which is not the folder on screen
 * when that is a search, Today or a merged inbox.
 *
 * `outboxId` is the queued message the draft was taken from, which the
 * window deletes from the Outbox once the draft is saved or sent.
 */
export async function openCompose(
  account: string,
  draft: Draft & { attachments?: string[]; identity?: string },
  savedUid?: number,
  outboxId?: number,
): Promise<void> {
  await call("open_compose", {
    draft: {
      account,
      to: draft.to,
      cc: draft.cc,
      bcc: draft.bcc,
      subject: draft.subject,
      body: draft.body,
      inReplyTo: draft.inReplyTo,
      references: draft.references,
      forwards: draft.forwards,
      savedUid: savedUid ?? null,
      outboxId: outboxId ?? null,
      identity: draft.identity,
      attachments: draft.attachments ?? [],
      addressedTo: draft.addressedTo ?? [],
      deliveredTo: draft.deliveredTo ?? [],
    },
  });
  setStatus("composing in a new window");
}

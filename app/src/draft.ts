/**
 * What a draft is, as far as the window is concerned.
 *
 * Building one — quoting, attribution, signature, threading headers — happens
 * in the core, where the reply settings and the account's signature already
 * are. Splitting and formatting recipients belongs to the field that shows
 * them, in `tokens.ts`. What is left here is the shape of a draft and the one
 * question the interface has to answer about it.
 */

export interface Draft {
  to: string[];
  cc: string[];
  bcc: string[];
  subject: string;
  body: string;
  /** Message-ID being replied to, for In-Reply-To. */
  inReplyTo?: string;
  /** Existing References chain, extended on reply. */
  references?: string;
  /** Message-ID being forwarded, marked `$Forwarded` once this is sent. */
  forwards?: string;
  /** For a reply or forward, every address the original names, for the warnings. */
  addressedTo?: string[];
  /** For a reply or forward, the original's Delivered-To. */
  deliveredTo?: string[];
}

/**
 * Reasons a draft cannot be sent, for display before anything is transmitted.
 *
 * Saving has no equivalent and must not: a draft is half-written by definition,
 * and the moment it most needs keeping is the moment it is least complete.
 */
export function draftProblems(d: Draft): string[] {
  const problems: string[] = [];
  if (d.to.length === 0 && d.cc.length === 0 && d.bcc.length === 0) {
    problems.push("no recipients");
  }
  if (!d.subject.trim()) problems.push("no subject");
  if (!d.body.trim()) problems.push("empty body");
  return problems;
}

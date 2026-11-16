# Ideas for working through tracker mail

Three concepts were sketched for working through a morning of issue-tracker
notifications, and for any other mail that arrives as many small messages about
a few subjects. `docs/mockups/conversation-concepts.html` shows all three, in
both themes, with a side-by-side comparison at the bottom.

- **A · Conversation reader**: shipped. A collapsed thread reads as one
  conversation, and the list's actions take it whole. See "Conversations" in
  the README.
- **B · Group by** and **C · Thread keys**: not built. They are described
  below so that the reasoning is not lost if a case for them turns up.

Neither of them is planned. They are not in `PLAN.md`, since that holds what
is missing or broken, not what might be nice.

## B · Group by

### The idea

A *grouping* gives each message a key, and the list shows one section per key:
the newest activity first, and each section's messages oldest first. Some
groupings are built in: conversation, sender, mailing list (`List-Id`) and day.
The rest come from the config, using the same pattern-and-capture style as
`message_links`:

```yaml
groupings:
  - name: issue
    from: '*@tracker.example.com'
    pattern: '#(\d+)'                  # matched against the subject by default
    label: '#$1'
  - name: team
    header: X-Ticket-Team      # the header's value is the key
  - name: severity
    header: X-Ticket-Priority
    order: ascending

bindings:
  index_normal:
    g1: group-by issue
```

You would switch grouping from a menu in the toolbar, with `zg`, or with
`:group-by <name>`. A folder would remember its grouping as it remembers `zt`
today. Conversations would become one entry in that menu, and the Messages and
Conversations switch would turn into the menu's button.

In the list:

- A section heading shows the key's label, the number of messages and of
  unread ones, and every highlight that matches a member.
- `za` folds a section, and `]g` and `[g` jump between sections.
- An action on a heading acts on the whole group, just as an action on a
  conversation's row does today.
- Messages that have no key go into a last section, "Not in a group".
- Grouped by team or severity, a section still folds its conversations as
  Orb does today. That makes a second level.

### Pros

- It is the most general of the three. Any header, any pattern and the
  built-in keys all work. Grouping by team or by severity is something no
  threading can do.
- Every message stays a row you can see. Nothing hides behind a caret.
- It fits the existing design of commands, menus and bindings, and the config
  style of `message_links` and highlight rules.
- It groups by issue even when a tracker's headers are poor, so it covers what
  C covers too.

### Cons

- It adds a second idea of "together" next to conversations. The two have to be
  one menu, not two features competing for the list, and conversations inside a
  group make a second level.
- The list is read from SQLite page by page. To sort by a group, every message
  has to store its key, and the keys have to be recomputed when a rule changes.
  That means a schema change and a backfill.
- A header key needs the header to be fetched. The highlight rules already do
  this, but only for messages that arrive after a rule first names the header.
- The tracker case needs configuration before it helps. A works without any.
- A group is still read message by message unless it gets A's reader too.
- Today's timeline and the iOS app would each need a sectioned list of their
  own.

### When to build it

Build it when someone needs a grouping that threading cannot give, such as by
team, by severity or by mailing list, and a search or saved view is not enough.

## C · Thread keys

### The idea

This adds no new view. A rule tells threading which messages belong to one
conversation when their headers do not say so:

```yaml
thread_keys:
  - name: issue
    from: '*@tracker.example.com'
    pattern: '#(\d+)'          # or: header: X-Ticket-Id
    key: 'issue-$1'
```

Threading still follows `In-Reply-To` and `References` first. A message with a
key then joins the conversation of the oldest message that has the same key.
`zt`, `za`, `gc` and the conversation reader all keep working unchanged. `gc`
could also follow a key across folders, so that a new comment finds its issue
in the Archive.

### Pros

- It is the smallest change: one rule type next to `subject_munges` and
  `message_links`, and no new UI.
- It fixes tools that thread badly: tools that send no `References`, tools that
  start a new chain at every change of state, and subjects that change.

### Cons

- Trackers that send the whole `References` chain, as many do, do not need
  it.
- It only fixes which messages belong together. Reading and acting stay as A
  made them.
- Changing a rule means threading every folder it applies to again.
- A pattern that matches too much merges unrelated mail without any warning.
  It would need debugging output like `:highlights` gives for highlight rules.

### When to build it

Build it when a real sender shows up whose mail about one subject splits into
several conversations.

## Comparison

| | A · Conversation reader | B · Group by | C · Thread keys |
|---|---|---|---|
| Fixes | Reading a conversation and acting on it as one | Seeing a folder by any key, not only by thread | Conversations that headers fail to join |
| Config for the tracker case | None | One `groupings` entry | One `thread_keys` entry |
| Helps beyond trackers | Every reply chain and mailing list | Sender, list, team, severity, day | Tools that send no `References` |
| New concepts for the user | None | A second kind of grouping | None visible: a config key |
| Cost | Medium | High: a stored key per message, a sectioned list, group actions, config, a menu | Low: one rule type, threading again after a config change |
| Status | Shipped | Not planned | Not planned |

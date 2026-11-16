/**
 * Rocket.Chat writes emoji as shortcodes (`:+1:`, `:pray:`), in messages and
 * in reactions alike. The common ones, as the server names them, with what
 * they draw; a shortcode not here is shown as written.
 */
const EMOJI: Record<string, string> = {
  "+1": "👍", thumbsup: "👍", "-1": "👎", thumbsdown: "👎", ok_hand: "👌", pray: "🙏", clap: "👏",
  wave: "👋", raised_hands: "🙌", muscle: "💪", point_up: "☝️", v: "✌️", handshake: "🤝",
  heart: "❤️", broken_heart: "💔", sparkling_heart: "💖", fire: "🔥", star: "⭐", sparkles: "✨",
  tada: "🎉", confetti_ball: "🎊", rocket: "🚀", trophy: "🏆", 100: "💯", bulb: "💡",
  eyes: "👀", thinking: "🤔", smile: "😄", smiley: "😃", grinning: "😀", grin: "😁", joy: "😂",
  rofl: "🤣", sweat_smile: "😅", laughing: "😆", wink: "😉", blush: "😊", innocent: "😇",
  slight_smile: "🙂", slightly_smiling_face: "🙂", upside_down: "🙃", heart_eyes: "😍",
  kissing_heart: "😘", yum: "😋", stuck_out_tongue: "😛", sunglasses: "😎", nerd: "🤓",
  smirk: "😏", neutral_face: "😐", expressionless: "😑", unamused: "😒", roll_eyes: "🙄",
  grimacing: "😬", relieved: "😌", pensive: "😔", sleepy: "😪", sleeping: "😴", mask: "😷",
  confused: "😕", worried: "😟", frowning: "☹️", open_mouth: "😮", hushed: "😯", astonished: "😲",
  flushed: "😳", cry: "😢", sob: "😭", scream: "😱", angry: "😠", rage: "😡", skull: "💀",
  facepalm: "🤦", shrug: "🤷", see_no_evil: "🙈", hugging: "🤗", partying_face: "🥳",
  white_check_mark: "✅", heavy_check_mark: "✔️", x: "❌", warning: "⚠️", no_entry: "⛔",
  question: "❓", exclamation: "❗", hourglass: "⌛", stopwatch: "⏱️", alarm_clock: "⏰",
  coffee: "☕", beer: "🍺", beers: "🍻", pizza: "🍕", cake: "🍰", gift: "🎁", balloon: "🎈",
  sunny: "☀️", rainbow: "🌈", zap: "⚡", snowflake: "❄️", umbrella: "☔", bug: "🐛",
  computer: "💻", phone: "📱", email: "📧", calendar: "📅", memo: "📝", pushpin: "📌",
  link: "🔗", lock: "🔒", key: "🔑", hammer: "🔨", wrench: "🔧", gear: "⚙️", chart: "📈",
  moneybag: "💰", dollar: "💵", euro: "💶", house: "🏠", car: "🚗", airplane: "✈️",
  checkered_flag: "🏁", rotating_light: "🚨", construction: "🚧", sos: "🆘", new: "🆕",
};

/** What a shortcode draws, with or without its colons; `null` for one unknown. */
export function emojiOf(code: string): string | null {
  return EMOJI[code.replace(/^:|:$/g, "")] ?? null;
}

/** A shortcode as it is drawn: the emoji when known, the code as written otherwise. */
export function drawEmoji(code: string): string {
  return emojiOf(code) ?? code;
}

/** Every `:shortcode:` in a text replaced by what it draws, when known. */
export function withEmoji(text: string): string {
  return text.replace(/:([a-z0-9_+-]+):/gi, (whole, code: string) => EMOJI[code.toLowerCase()] ?? whole);
}

/** The shortcodes that start with `prefix`, for completion; at most `limit`. */
export function emojiMatching(prefix: string, limit = 8): { code: string; glyph: string }[] {
  const p = prefix.toLowerCase();
  const seen = new Set<string>();
  const out: { code: string; glyph: string }[] = [];
  for (const [code, glyph] of Object.entries(EMOJI)) {
    if (!code.startsWith(p) || seen.has(glyph)) continue;
    seen.add(glyph);
    out.push({ code, glyph });
    if (out.length >= limit) break;
  }
  return out;
}

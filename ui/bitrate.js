// Bitrate requests, the viewer's side (docs/design.md §8.13): the most a viewer may ask a host's stream to carry,
// and what the host's answer means. The Watch page and the panel over the player both use these.

import { t } from './i18n.js';

/** What a viewer may ask for, Mbps, highest first (those under the host's setting are offered). */
const STEPS = [50, 30, 20, 15, 10, 8, 5, 3, 2];
/** At most this many under the host's setting: with "the host's setting" they fill two rows of the panel's chips. */
const OFFERED = 7;

/** kbit/s as Mbps for the page: "8", "1.5". */
export function mbpsText(kbps) {
  const mbps = kbps / 1000;
  return Number.isInteger(mbps) ? String(mbps) : mbps.toFixed(1);
}

/** What a viewer can ask for, kbit/s: 0 (the host's setting), then the steps under the host's setting `ceilingKbps`,
 *  highest first. What is asked for now (`wantedKbps`) stays among them, even above a setting lowered since. */
export function bitrateChoices(ceilingKbps, wantedKbps) {
  const under = STEPS.map((mbps) => mbps * 1000).filter((kbps) => !ceilingKbps || kbps < ceilingKbps).slice(0, OFFERED);
  if (wantedKbps > 0 && !under.includes(wantedKbps)) {
    if (under.length === OFFERED) under.pop();
    under.push(wantedKbps);
    under.sort((a, b) => b - a);
  }
  return [0, ...under];
}

/** The host's setting for the stream watched, kbit/s: as its latest answer gave it, else as its live record did. */
export function ceilingOf(bitrate) {
  return bitrate?.link?.ceilingKbps || (bitrate?.hostMbps > 0 ? bitrate.hostMbps * 1000 : 0);
}

/** What the host's stream runs at, and why, from the watch status' `bitrate` (null: nothing to say yet). */
export function bitrateNote(bitrate) {
  const link = bitrate?.link;
  if (!link) return null;
  if (link.waiting || link.kbps == null) return t('watch.brWaiting');
  const host = mbpsText(link.ceilingKbps);
  const mbps = mbpsText(link.kbps);
  if (!link.taken) return t('watch.brOff', { mbps: host });
  if (link.kbps >= link.ceilingKbps) return t('watch.brAtHost', { mbps: host });
  if (link.wantedKbps > 0 && link.kbps >= Math.min(link.wantedKbps, link.ceilingKbps)) return t('watch.brAsked', { mbps, host });
  return t('watch.brOther', { mbps, host });
}

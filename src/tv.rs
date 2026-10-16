//! /tv's channels: the downloaded archive played as television, in a
//! shuffled order nobody picks. Each channel (all platforms, or one) is a
//! live schedule: it runs whether anyone watches or not, so tuning in lands
//! midway through whatever is on, and flipping away and back finds the
//! channel where it would be by now. The schedule lives in memory and starts
//! afresh with the app, or after an hour with nobody watching.
//!
//! What airs next follows a few rules, as a station's programming would:
//!
//! - Each platform's posts are a deck, shuffled and dealt until it runs out:
//!   no post airs twice until every other one has aired.
//! - A creator doesn't air again within `AUTHOR_GAP` shows when the deck
//!   has anyone else left to deal.
//! - On the all-platforms channel, each platform gets a share of the
//!   *airtime* in proportion to how many of its posts are downloaded.
//!   Picking by count instead would hand most of the day to YouTube, whose
//!   posts run several times longer than TikTok's and Instagram's.
//! - A video longer than `MAX_AIRTIME` airs a stretch of it, from a random
//!   point, like a show joined in progress; its page has the whole thing.
//! - A photo post airs for a few seconds a picture (`photo_airtime`).
//! - Every `BREAK_EVERY` of airtime, at the end of a show, the station
//!   takes a break: one of its bumpers (short idents and "up next" cards,
//!   assets/blender/bumpers.py), dealt from a shuffled deck of its own. They
//!   are on the schedule like the shows, so tuning in can land in one.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Airable;
use crate::{loudness, player};

/// The longest anything airs, in seconds.
const MAX_AIRTIME: f64 = 600.0;

/// A photo post airs for this long a picture, within the bounds below.
const PHOTO_SECONDS_EACH: f64 = 5.0;
const PHOTO_SHORTEST: f64 = 10.0;
const PHOTO_LONGEST: f64 = 40.0;

/// How much airtime goes by between station breaks, in seconds: a few
/// minutes, so a channel of 15-second posts isn't all bumpers, and a long
/// video's stretch has one after it.
const BREAK_EVERY: f64 = 240.0;

/// How many shows go by before a creator airs again, when the deck allows.
const AUTHOR_GAP: usize = 3;

/// A show doesn't air again within this many shows while its channel has
/// anything else to air: on the all-platforms channel, a platform with a
/// handful of short posts would otherwise repeat them to fill its share.
const REPEAT_GAP: u64 = 30;

/// With nobody watching for this long, a channel starts over rather than
/// working out everything that would have aired meanwhile.
const RESTART_AFTER: f64 = 3600.0;

/// How long a show stays on the schedule after it ends, in seconds, so a
/// page running a little behind can still ask for what follows it.
const KEEP_AIRED: f64 = 600.0;

/// How far past what's on now a page may ask for the schedule, in shows.
const MOST_AHEAD: u64 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    All,
    YouTube,
    TikTok,
    Instagram,
    X,
}

impl Channel {
    /// In the order of their numbers, from 1.
    pub const ALL: [Channel; 5] = [
        Channel::All,
        Channel::YouTube,
        Channel::TikTok,
        Channel::Instagram,
        Channel::X,
    ];

    /// Its name in URLs: `/tv/<slug>`.
    pub fn slug(self) -> &'static str {
        match self {
            Channel::All => "all",
            Channel::YouTube => "youtube",
            Channel::TikTok => "tiktok",
            Channel::Instagram => "instagram",
            Channel::X => "x",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Channel::All => "All",
            Channel::YouTube => "YouTube",
            Channel::TikTok => "TikTok",
            Channel::Instagram => "Instagram",
            Channel::X => "X",
        }
    }

    /// Its number on the remote, from 1.
    pub fn number(self) -> usize {
        Channel::ALL.iter().position(|c| *c == self).unwrap_or(0) + 1
    }

    pub fn from_slug(slug: &str) -> Option<Channel> {
        Channel::ALL.into_iter().find(|c| c.slug() == slug)
    }

    fn carries(self, platform: &str) -> bool {
        self == Channel::All || self.slug() == platform
    }
}

/// A post a channel can air.
#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    pub platform: String,
    pub video_id: String,
    pub author: Option<String>,
    /// How long it would air in full, in seconds: a video's length, or a
    /// photo post's `photo_airtime`.
    pub length: f64,
    /// A photo post's pictures (and a carousel's videos); 0 for a video.
    pub slides: usize,
    /// The gain that brings its sound to the TV's level (`loudness.rs`).
    pub gain_db: f64,
}

impl Show {
    /// `airable` as a show, unless it can't air: there's nothing in its
    /// files to play, or it's a video of unknown length.
    pub fn from_airable(airable: &Airable) -> Option<Show> {
        let (length, slides) = match player::playable(&airable.files)? {
            player::Playable::Video { .. } => (
                airable.duration_seconds.filter(|seconds| *seconds >= 1.0)?,
                0,
            ),
            player::Playable::Photos { slides, .. } => (photo_airtime(slides.len()), slides.len()),
        };
        Some(Show {
            platform: airable.platform.clone(),
            video_id: airable.video_id.clone(),
            author: airable.author_name.clone(),
            length,
            slides,
            gain_db: loudness::gain_db(airable.integrated_lufs, airable.true_peak_dbtp),
        })
    }

    fn key(&self) -> Key {
        (self.platform.clone(), self.video_id.clone())
    }

    fn creator(&self) -> Option<(String, String)> {
        self.author
            .as_ref()
            .map(|author| (self.platform.clone(), author.to_lowercase()))
    }
}

/// How long a photo post with `slides` pictures airs, in seconds.
fn photo_airtime(slides: usize) -> f64 {
    #[expect(clippy::cast_precision_loss, reason = "a post has a few pictures")]
    let slides = slides as f64;
    (slides * PHOTO_SECONDS_EACH).clamp(PHOTO_SHORTEST, PHOTO_LONGEST)
}

type Key = (String, String);

/// One of the station's bumpers: a short clip it airs between shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Bumper {
    pub name: String,
    /// How long it runs, in seconds.
    pub seconds: f64,
}

/// What a slot airs.
#[derive(Debug, Clone, PartialEq)]
pub enum Program {
    Show(Show),
    Break(Bumper),
}

/// One show's (or break's) turn on air.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    /// Its place on the channel's schedule, counting up from 0.
    pub seq: u64,
    pub program: Program,
    /// When it goes on air, unix seconds.
    pub starts_at: f64,
    /// Where in the media it starts, in seconds: past 0 only for a long
    /// video airing a stretch.
    pub from: f64,
    /// How long it airs, in seconds.
    pub airtime: f64,
}

impl Slot {
    pub fn ends_at(&self) -> f64 {
        self.starts_at + self.airtime
    }

    /// The show it airs, unless it's a break.
    pub fn show(&self) -> Option<&Show> {
        match &self.program {
            Program::Show(show) => Some(show),
            Program::Break(_) => None,
        }
    }
}

/// One channel's schedule: the decks it deals from and the shows coming up.
#[derive(Debug)]
pub struct Lineup {
    channel: Channel,
    rng: Rng,
    /// Everything the channel can air now, by key.
    shows: HashMap<Key, Show>,
    /// How many of `shows` each platform has, sorted by platform.
    library: Vec<(String, usize)>,
    /// What's left of each platform's deck; the end is dealt first.
    decks: HashMap<String, Vec<Key>>,
    /// Each platform's airtime so far, which the all-platforms channel
    /// evens out.
    aired: HashMap<String, f64>,
    /// The creators of the last `AUTHOR_GAP` shows dealt.
    recent: VecDeque<Option<(String, String)>>,
    /// The last show dealt, so a fresh deck doesn't start with it.
    last: Option<Key>,
    /// Each show's `seq` when it last aired.
    last_seq: HashMap<Key, u64>,
    slots: VecDeque<Slot>,
    next_seq: u64,
    /// The station's bumpers, and what's left of their deck (indexes into
    /// it; the end is dealt first).
    bumpers: Vec<Bumper>,
    breaks: Vec<usize>,
    /// The last bumper dealt, so a fresh deck doesn't start with it.
    last_break: Option<usize>,
    /// Airtime since the last break, in seconds.
    since_break: f64,
}

impl Lineup {
    pub fn new(channel: Channel, seed: u64, bumpers: Vec<Bumper>) -> Lineup {
        Lineup {
            channel,
            rng: Rng(seed),
            shows: HashMap::new(),
            library: Vec::new(),
            decks: HashMap::new(),
            aired: HashMap::new(),
            recent: VecDeque::new(),
            last: None,
            last_seq: HashMap::new(),
            slots: VecDeque::new(),
            next_seq: 0,
            bumpers,
            breaks: Vec::new(),
            last_break: None,
            since_break: 0.0,
        }
    }

    /// Bring the channel up to date with what's downloaded. A new post is
    /// shuffled into what's left of its platform's deck, so it airs before
    /// the deck runs out; a post that's gone is taken out.
    pub fn refresh(&mut self, shows: impl IntoIterator<Item = Show>) {
        let shows: HashMap<Key, Show> = shows
            .into_iter()
            .filter(|show| self.channel.carries(&show.platform))
            .map(|show| (show.key(), show))
            .collect();
        let mut new: Vec<&Key> = shows
            .keys()
            .filter(|key| !self.shows.contains_key(*key))
            .collect();
        // HashMap order changes from run to run; the schedule mustn't.
        new.sort();
        for key in new {
            let deck = self.decks.entry(key.0.clone()).or_default();
            let at = self.rng.below(deck.len() + 1);
            deck.insert(at, key.clone());
        }
        for deck in self.decks.values_mut() {
            deck.retain(|key| shows.contains_key(key));
        }
        // A show already scheduled keeps its turn, with its latest gain.
        for slot in &mut self.slots {
            if let Program::Show(on) = &mut slot.program
                && let Some(show) = shows.get(&on.key())
            {
                on.gain_db = show.gain_db;
            }
        }
        let mut library: HashMap<String, usize> = HashMap::new();
        for show in shows.values() {
            *library.entry(show.platform.clone()).or_default() += 1;
        }
        let mut library: Vec<(String, usize)> = library.into_iter().collect();
        library.sort_unstable();
        self.share_fairly(&library);
        self.library = library;
        self.shows = shows;
    }

    /// Keep the all-platforms channel's airtime balance fair as the library
    /// changes, as weighted fair queueing does: a platform's posts arriving
    /// late would otherwise owe it hours of airtime, and it would win every
    /// deal (airing its only post again and again) until it had caught up.
    /// A platform that joins starts level with the others, and one whose
    /// number of posts changes keeps its place.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a share of a few thousand posts"
    )]
    fn share_fairly(&mut self, library: &[(String, usize)]) {
        let level = self
            .library
            .iter()
            .filter_map(|(platform, count)| Some(self.aired.get(platform)? / *count as f64))
            .reduce(f64::min)
            .unwrap_or(0.0);
        let before: HashMap<&str, usize> = self
            .library
            .iter()
            .map(|(platform, count)| (platform.as_str(), *count))
            .collect();
        let mut aired = HashMap::new();
        for (platform, count) in library {
            let now = match (self.aired.get(platform), before.get(platform.as_str())) {
                (Some(aired), Some(was)) => aired / *was as f64 * *count as f64,
                _ => level * *count as f64,
            };
            aired.insert(platform.clone(), now);
        }
        self.aired = aired;
    }

    /// Up to `count` slots from `from` on (a slot's `seq`), or from what's
    /// on air at `now` when `from` is None or already gone. Works out the
    /// schedule as far as that needs. Empty when the channel has nothing to
    /// air.
    pub fn slots(&mut self, now: f64, from: Option<u64>, count: usize) -> Vec<Slot> {
        if self
            .slots
            .back()
            .is_none_or(|last| last.ends_at() < now - RESTART_AFTER)
        {
            self.slots.clear();
            // Tuning in to a channel that's been on all along: somewhere in
            // the middle of a show, never in its last moments.
            let Some(mut first) = self.deal(0.0) else {
                return Vec::new();
            };
            first.starts_at = now - self.rng.unit() * first.airtime * 0.8;
            self.slots.push_back(first);
        }
        while let Some(last) = self.slots.back()
            && last.ends_at() <= now
        {
            let starts_at = last.ends_at();
            let Some(next) = self.next(starts_at) else {
                return Vec::new();
            };
            self.slots.push_back(next);
        }
        while self
            .slots
            .front()
            .is_some_and(|slot| slot.ends_at() < now - KEEP_AIRED)
        {
            self.slots.pop_front();
        }
        let Some(on_air) = self.slots.iter().find(|slot| slot.ends_at() > now) else {
            return Vec::new();
        };
        let on_air = on_air.seq;
        let first_kept = self.slots.front().map_or(on_air, |slot| slot.seq);
        let start = match from {
            Some(seq) if seq >= first_kept => seq.min(on_air + MOST_AHEAD),
            _ => on_air,
        };
        let end = start + u64::try_from(count).unwrap_or(u64::MAX);
        while let Some(last) = self.slots.back()
            && last.seq + 1 < end
        {
            let starts_at = last.ends_at();
            let Some(next) = self.next(starts_at) else {
                break;
            };
            self.slots.push_back(next);
        }
        self.slots
            .iter()
            .filter(|slot| (start..end).contains(&slot.seq))
            .cloned()
            .collect()
    }

    /// The next slot, starting at `starts_at`: a break when one is due,
    /// otherwise the next show.
    fn next(&mut self, starts_at: f64) -> Option<Slot> {
        let after_show = self.slots.back().is_some_and(|slot| slot.show().is_some());
        if after_show && self.since_break >= BREAK_EVERY && !self.bumpers.is_empty() {
            return self.deal_break(starts_at);
        }
        self.deal(starts_at)
    }

    /// A break's slot, starting at `starts_at`: the next bumper from the
    /// deck, shuffling a fresh one when it's run out, never the last one
    /// again.
    fn deal_break(&mut self, starts_at: f64) -> Option<Slot> {
        if self.breaks.is_empty() {
            let mut deck: Vec<usize> = (0..self.bumpers.len()).collect();
            self.rng.shuffle(&mut deck);
            // Dealt from the end: the last one aired mustn't be there.
            let last = deck.len() - 1;
            if last > 0 && deck.last() == self.last_break.as_ref() {
                deck.swap(0, last);
            }
            self.breaks = deck;
        }
        let at = self.breaks.pop()?;
        let bumper = self.bumpers.get(at)?.clone();
        self.last_break = Some(at);
        self.since_break = 0.0;
        let seq = self.next_seq;
        self.next_seq += 1;
        Some(Slot {
            seq,
            starts_at,
            from: 0.0,
            airtime: bumper.seconds,
            program: Program::Break(bumper),
        })
    }

    /// The next show's slot, starting at `starts_at`.
    fn deal(&mut self, starts_at: f64) -> Option<Slot> {
        let platform = self.next_platform()?;
        let key = self.next_from(&platform)?;
        self.last_seq.insert(key.clone(), self.next_seq);
        let show = self.shows.get(&key)?.clone();
        let (from, airtime) = if show.slides == 0 && show.length > MAX_AIRTIME {
            let from = (self.rng.unit() * (show.length - MAX_AIRTIME)).floor();
            (from, MAX_AIRTIME)
        } else {
            (0.0, show.length)
        };
        *self.aired.entry(platform).or_default() += airtime;
        self.since_break += airtime;
        self.recent.push_back(show.creator());
        while self.recent.len() > AUTHOR_GAP {
            self.recent.pop_front();
        }
        self.last = Some(key);
        let seq = self.next_seq;
        self.next_seq += 1;
        Some(Slot {
            seq,
            program: Program::Show(show),
            starts_at,
            from,
            airtime,
        })
    }

    /// Which platform's deck deals next: on a one-platform channel, its
    /// own; on the all-platforms channel, the one furthest behind its share
    /// of the airtime that has a show to air that hasn't aired lately. A
    /// platform passed over for that gives up its turn, rather than being
    /// owed the airtime and taking it all at once later.
    #[expect(
        clippy::cast_precision_loss,
        reason = "a share of a few thousand posts"
    )]
    fn next_platform(&mut self) -> Option<String> {
        let behind = |aired: &HashMap<String, f64>, platform: &str, count: usize| {
            aired.get(platform).copied().unwrap_or(0.0) / count as f64
        };
        let mut order = self.library.clone();
        // Stable: ties go to the first by name, so a seed replays.
        order.sort_by(|(a, an), (b, bn)| {
            behind(&self.aired, a, *an).total_cmp(&behind(&self.aired, b, *bn))
        });
        let chosen = order
            .iter()
            .position(|(platform, _)| self.has_rested(platform))
            .unwrap_or(0);
        let (platform, count) = order.get(chosen)?.clone();
        let level = behind(&self.aired, &platform, count);
        for (passed, count) in &order[..chosen] {
            self.aired.insert(passed.clone(), level * *count as f64);
        }
        Some(platform)
    }

    /// Whether `key` hasn't aired within the last `REPEAT_GAP` shows.
    fn rested(&self, key: &Key) -> bool {
        self.last_seq
            .get(key)
            .is_none_or(|seq| self.next_seq - seq > REPEAT_GAP)
    }

    /// Whether `platform`'s deck (or the fresh one it would shuffle) has a
    /// show that has rested.
    fn has_rested(&self, platform: &str) -> bool {
        match self.decks.get(platform).filter(|deck| !deck.is_empty()) {
            Some(deck) => deck.iter().any(|key| self.rested(key)),
            None => self
                .shows
                .keys()
                .any(|key| key.0 == platform && self.rested(key)),
        }
    }

    /// Deal the next show from `platform`'s deck, shuffling a fresh one when
    /// it's run out: the first one dealt whose creator hasn't aired lately,
    /// or failing that one by anyone but the last show's creator.
    fn next_from(&mut self, platform: &str) -> Option<Key> {
        if self.decks.get(platform).is_none_or(Vec::is_empty) {
            let mut deck: Vec<Key> = self
                .shows
                .keys()
                .filter(|(p, _)| p == platform)
                .cloned()
                .collect();
            deck.sort();
            self.rng.shuffle(&mut deck);
            self.decks.insert(platform.to_owned(), deck);
        }
        let deck = self.decks.get(platform)?;
        let creator = |key: &Key| self.shows.get(key).and_then(Show::creator);
        let last_creator = self.recent.back().cloned().flatten();
        let fresh =
            |key: &Key| creator(key).is_none_or(|c| !self.recent.iter().flatten().any(|r| *r == c));
        let not_last = |key: &Key| creator(key).is_none_or(|c| Some(c) != last_creator);
        let not_again = |key: &Key| self.last.as_ref() != Some(key);
        // A creator with more than half of what's left would have to air
        // twice in a row by the end of the deck unless dealt whenever
        // allowed.
        let mut left: HashMap<(String, String), usize> = HashMap::new();
        for c in deck.iter().filter_map(creator) {
            *left.entry(c).or_default() += 1;
        }
        let crowded = left
            .into_iter()
            .filter(|(c, count)| count * 2 > deck.len() && Some(c) != last_creator.as_ref())
            .map(|(c, _)| c)
            .next();
        let at = crowded
            .and_then(|crowded| {
                deck.iter()
                    .rposition(|key| creator(key).as_ref() == Some(&crowded))
            })
            .or_else(|| {
                deck.iter()
                    .rposition(|key| self.rested(key) && fresh(key) && not_again(key))
            })
            .or_else(|| {
                deck.iter()
                    .rposition(|key| self.rested(key) && not_last(key) && not_again(key))
            })
            .or_else(|| deck.iter().rposition(|key| fresh(key) && not_again(key)))
            .or_else(|| deck.iter().rposition(|key| not_last(key) && not_again(key)))
            .or_else(|| deck.iter().rposition(not_again))
            .unwrap_or(deck.len().checked_sub(1)?);
        Some(self.decks.get_mut(platform)?.remove(at))
    }
}

/// Every channel's lineup, for the web UI.
pub struct Tv {
    seed: u64,
    bumpers: Vec<Bumper>,
    lineups: Mutex<HashMap<Channel, Lineup>>,
}

impl Tv {
    /// Channels whose schedules are shuffled by `seed`, with `bumpers` in
    /// their breaks.
    pub fn new(seed: u64, bumpers: Vec<Bumper>) -> Tv {
        Tv {
            seed,
            bumpers,
            lineups: Mutex::new(HashMap::new()),
        }
    }

    /// `channel`'s slots, as `Lineup::slots` gives them, after bringing it
    /// up to date with `shows`.
    pub fn slots(
        &self,
        channel: Channel,
        shows: Vec<Show>,
        now: f64,
        from: Option<u64>,
        count: usize,
    ) -> Vec<Slot> {
        let mut lineups = self.lineups.lock().unwrap_or_else(PoisonError::into_inner);
        // Each channel shuffles differently.
        let seed = self.seed ^ (channel.number() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let lineup = lineups
            .entry(channel)
            .or_insert_with(|| Lineup::new(channel, seed, self.bumpers.clone()));
        lineup.refresh(shows);
        lineup.slots(now, from, count)
    }
}

/// A seed for this run's schedules: the clock, in nanoseconds.
pub fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The time now, in unix seconds with a fraction, as the schedule keeps it.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// `SplitMix64`: small, fast and seedable, so a schedule can be replayed in
/// tests. Nothing here needs more than that.
#[derive(Debug)]
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// From 0 up to, not including, `n` (which must be above 0).
    fn below(&mut self, n: usize) -> usize {
        let n = n as u64;
        usize::try_from(self.next() % n).unwrap_or(0)
    }

    /// From 0 up to, not including, 1.
    fn unit(&mut self) -> f64 {
        // 2^53: an f64 holds 53 random bits exactly.
        const SPAN: f64 = 9_007_199_254_740_992.0;
        #[expect(clippy::cast_precision_loss, reason = "53 random bits, exactly")]
        let bits = (self.next() >> 11) as f64;
        bits / SPAN
    }

    /// Fisher–Yates.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn show(platform: &str, id: usize, author: &str, length: f64) -> Show {
        Show {
            platform: platform.into(),
            video_id: format!("{platform}{id}"),
            author: Some(author.into()),
            length,
            slides: 0,
            gain_db: 0.0,
        }
    }

    /// `count` shows on `platform` by `authors` in turn, each `length` long.
    fn library(platform: &str, count: usize, authors: &[&str], length: f64) -> Vec<Show> {
        (0..count)
            .map(|i| show(platform, i, authors[i % authors.len()], length))
            .collect()
    }

    fn lineup(channel: Channel, seed: u64, shows: &[Show]) -> Lineup {
        let mut lineup = Lineup::new(channel, seed, Vec::new());
        lineup.refresh(shows.iter().cloned());
        lineup
    }

    fn deal(lineup: &mut Lineup, count: usize) -> Vec<Slot> {
        (0..count).map(|_| lineup.deal(0.0).unwrap()).collect()
    }

    fn ids(slots: &[Slot]) -> Vec<String> {
        slots
            .iter()
            .map(|s| s.show().unwrap().video_id.clone())
            .collect()
    }

    #[test]
    fn every_show_airs_once_a_cycle() {
        let shows = library("tiktok", 40, &["a", "b", "c", "d", "e"], 30.0);
        for seed in 0..50 {
            let mut lineup = lineup(Channel::TikTok, seed, &shows);
            for _cycle in 0..3 {
                let aired: HashSet<String> = ids(&deal(&mut lineup, 40)).into_iter().collect();
                assert_eq!(aired.len(), 40, "seed {seed}");
            }
        }
    }

    #[test]
    fn a_show_never_airs_twice_in_a_row() {
        // Even across the end of a deck, and with only two shows.
        for seed in 0..200 {
            let shows = library("youtube", 2, &["a"], 30.0);
            let aired = ids(&deal(&mut lineup(Channel::YouTube, seed, &shows), 50));
            assert!(aired.windows(2).all(|w| w[0] != w[1]), "seed {seed}");
        }
    }

    #[test]
    fn creators_are_spaced_out() {
        // A creator who made half the library can't always be kept three
        // shows apart, but never airs twice in a row while anyone else is
        // left in the deck.
        let mut shows = library("tiktok", 20, &["b", "c", "d", "e"], 30.0);
        shows.extend((20..40).map(|i| show("tiktok", i, "a", 30.0)));
        for seed in 0..50 {
            let slots = deal(&mut lineup(Channel::TikTok, seed, &shows), 400);
            let authors: Vec<_> = slots
                .iter()
                .map(|s| s.show().unwrap().author.clone())
                .collect();
            let repeats = authors.windows(2).filter(|w| w[0] == w[1]).count();
            assert_eq!(repeats, 0, "seed {seed}");
        }
        // Shaped like a real library, where most creators made one or two
        // of the posts and a few made many: none airs within three shows
        // of the last.
        let mut shows = library("tiktok", 80, &["a", "b", "c", "d"], 30.0);
        shows.extend((80..300).map(|i| show("tiktok", i, &format!("{}", i / 2), 30.0)));
        let mut worst = 0;
        for seed in 0..20 {
            let slots = deal(&mut lineup(Channel::TikTok, seed, &shows), 1500);
            let authors: Vec<_> = slots
                .iter()
                .map(|s| s.show().unwrap().author.clone())
                .collect();
            let close = authors
                .windows(AUTHOR_GAP + 1)
                .filter(|w| w[1..].contains(&w[0]))
                .count();
            worst = worst.max(close);
        }
        // Only at a deck's very end, when everyone left aired lately.
        assert!(worst <= 2, "{worst} close repeats");
    }

    #[test]
    fn all_shares_airtime_by_library_size() {
        // YouTube's posts run ten times TikTok's, but it has a third as many.
        let mut shows = library("youtube", 10, &["y1", "y2", "y3"], 300.0);
        shows.extend(library("tiktok", 30, &["t1", "t2", "t3"], 30.0));
        shows.extend(library("instagram", 20, &["i1", "i2", "i3"], 20.0));
        for seed in 0..10 {
            let slots = deal(&mut lineup(Channel::All, seed, &shows), 3000);
            let total: f64 = slots.iter().map(|s| s.airtime).sum();
            for (platform, share) in [("youtube", 10.0), ("tiktok", 30.0), ("instagram", 20.0)] {
                let aired: f64 = slots
                    .iter()
                    .filter(|s| s.show().unwrap().platform == platform)
                    .map(|s| s.airtime)
                    .sum();
                let off = (aired / total - share / 60.0).abs();
                assert!(off < 0.02, "seed {seed}: {platform} off its share by {off}");
            }
        }
    }

    #[test]
    fn a_platform_arriving_late_joins_its_share() {
        // Hours of YouTube, then Instagram's first post finishes downloading.
        let mut shows = library("youtube", 60, &["y1", "y2", "y3", "y4"], 300.0);
        let mut lineup = lineup(Channel::All, 5, &shows);
        deal(&mut lineup, 200);
        shows.push(show("instagram", 0, "i", 20.0));
        lineup.refresh(shows.clone());
        let slots = deal(&mut lineup, 22);
        let instagram = slots
            .iter()
            .filter(|s| s.show().unwrap().platform == "instagram")
            .count();
        // Its one post airs, then rests while YouTube carries on, rather
        // than airing again and again to make up its share.
        assert!((1..=3).contains(&instagram), "{instagram}");
        assert!(ids(&slots).windows(2).all(|w| w[0] != w[1]));
        // More of its posts arrive: it gets more airtime, not a burst.
        shows.extend(
            library("instagram", 30, &["i1", "i2", "i3"], 20.0)
                .into_iter()
                .skip(1),
        );
        lineup.refresh(shows);
        let slots = deal(&mut lineup, 60);
        let run = slots
            .iter()
            .fold((0, 0), |(longest, now), s| {
                let now = if s.show().unwrap().platform == "instagram" {
                    now + 1
                } else {
                    0
                };
                (longest.max(now), now)
            })
            .0;
        assert!(run <= 20, "{run} Instagram posts in a row");
    }

    #[test]
    fn a_seed_replays_the_same_schedule() {
        let shows = library("tiktok", 30, &["a", "b", "c"], 30.0);
        let first = ids(&deal(&mut lineup(Channel::TikTok, 7, &shows), 60));
        let again = ids(&deal(&mut lineup(Channel::TikTok, 7, &shows), 60));
        let other = ids(&deal(&mut lineup(Channel::TikTok, 8, &shows), 60));
        assert_eq!(first, again);
        assert_ne!(first, other);
    }

    #[test]
    fn long_videos_air_a_stretch() {
        let shows = vec![
            show("youtube", 0, "a", 5370.0),
            show("youtube", 1, "b", 90.0),
        ];
        for seed in 0..50 {
            for slot in deal(&mut lineup(Channel::YouTube, seed, &shows), 4) {
                if slot.show().unwrap().length > MAX_AIRTIME {
                    assert!((slot.airtime - MAX_AIRTIME).abs() < f64::EPSILON);
                    assert!(
                        slot.from >= 0.0 && slot.from + slot.airtime <= slot.show().unwrap().length
                    );
                } else {
                    assert!(slot.from == 0.0 && (slot.airtime - 90.0).abs() < f64::EPSILON);
                }
            }
        }
    }

    #[test]
    fn tuning_in_lands_on_what_is_on_air() {
        let shows = library("tiktok", 20, &["a", "b", "c"], 30.0);
        let mut lineup = lineup(Channel::TikTok, 1, &shows);
        let now = 1_000_000.0;
        let slots = lineup.slots(now, None, 4);
        assert_eq!(slots.len(), 4);
        assert!(slots[0].starts_at <= now && now < slots[0].ends_at());
        // Back to back, numbered in order.
        for pair in slots.windows(2) {
            assert!((pair[1].starts_at - pair[0].ends_at()).abs() < 1e-9);
            assert_eq!(pair[1].seq, pair[0].seq + 1);
        }
        // Later, it has moved on, and the same slots are still the same.
        let later = lineup.slots(now + 45.0, None, 2);
        assert!(later[0].starts_at <= now + 45.0 && now + 45.0 < later[0].ends_at());
        assert_eq!(
            later[0],
            slots[usize::try_from(later[0].seq - slots[0].seq).unwrap()]
        );
        // A page carries on from what it has.
        let next = lineup.slots(now + 45.0, Some(slots[3].seq + 1), 3);
        assert_eq!(next[0].seq, slots[3].seq + 1);
        assert!((next[0].starts_at - slots[3].ends_at()).abs() < 1e-9);
        // Nobody watching for over an hour: it starts over, mid-show.
        let much_later = now + RESTART_AFTER + 1000.0;
        let restarted = lineup.slots(much_later, None, 1);
        assert!(restarted[0].starts_at <= much_later && much_later < restarted[0].ends_at());
    }

    #[test]
    fn new_downloads_air_within_a_cycle_and_gone_ones_stop() {
        let mut shows = library("tiktok", 20, &["a", "b", "c", "d"], 30.0);
        let mut lineup = lineup(Channel::TikTok, 3, &shows);
        deal(&mut lineup, 5);
        shows.push(show("tiktok", 99, "z", 30.0));
        let gone = shows.remove(0);
        lineup.refresh(shows.clone());
        // 15 left in this deck, plus the new one.
        let aired = ids(&deal(&mut lineup, 16));
        assert!(aired.contains(&"tiktok99".to_string()));
        let later = ids(&deal(&mut lineup, 100));
        assert!(!aired.contains(&gone.video_id) && !later.contains(&gone.video_id));
    }

    #[test]
    fn channels_carry_their_platforms() {
        let mut shows = library("youtube", 3, &["a"], 30.0);
        shows.extend(library("tiktok", 3, &["b"], 30.0));
        let mut tiktok = lineup(Channel::TikTok, 0, &shows);
        assert!(
            deal(&mut tiktok, 9)
                .iter()
                .all(|s| s.show().unwrap().platform == "tiktok")
        );
        let mut instagram = lineup(Channel::Instagram, 0, &shows);
        assert_eq!(instagram.slots(0.0, None, 3), Vec::<Slot>::new());
        assert_eq!(Channel::from_slug("tiktok"), Some(Channel::TikTok));
        assert_eq!(Channel::Instagram.number(), 4);
    }

    #[test]
    fn photo_posts_air_a_few_seconds_a_picture() {
        let airable = |files: &[&str], duration| Airable {
            platform: "tiktok".into(),
            video_id: "1".into(),
            kind: "photo".into(),
            author_name: None,
            files: files.iter().map(ToString::to_string).collect(),
            duration_seconds: duration,
            integrated_lufs: Some(-26.0),
            true_peak_dbtp: Some(-12.0),
        };
        let one = Show::from_airable(&airable(&["image-01.jpg", "sound.mp3"], None)).unwrap();
        assert!((one.length - PHOTO_SHORTEST).abs() < f64::EPSILON);
        assert_eq!(one.slides, 1);
        assert!((one.gain_db - 10.0).abs() < 1e-9);
        let names: Vec<String> = (1..=12).map(|n| format!("image-{n:02}.jpg")).collect();
        let many: Vec<&str> = names.iter().map(String::as_str).collect();
        let many = Show::from_airable(&airable(&many, None)).unwrap();
        assert!((many.length - PHOTO_LONGEST).abs() < f64::EPSILON);
        // A video airs for its length, and can't without one.
        let video = Show::from_airable(&airable(&["video.mp4"], Some(31.5))).unwrap();
        assert!((video.length - 31.5).abs() < f64::EPSILON);
        assert_eq!(video.slides, 0);
        assert!(Show::from_airable(&airable(&["video.mp4"], None)).is_none());
        assert!(Show::from_airable(&airable(&["thumbnail.jpg"], Some(9.0))).is_none());
    }

    fn bumpers() -> Vec<Bumper> {
        ["splat", "up-next", "slime"]
            .iter()
            .map(|name| Bumper {
                name: (*name).into(),
                seconds: 5.0,
            })
            .collect()
    }

    /// The first `count` slots of `lineup`'s schedule, read as a page would.
    fn schedule(lineup: &mut Lineup, count: usize) -> Vec<Slot> {
        let mut slots = lineup.slots(0.0, None, 1);
        while slots.len() < count {
            let last = slots.last().unwrap().clone();
            slots.extend(lineup.slots(last.starts_at, Some(last.seq + 1), 10));
        }
        slots.truncate(count);
        slots
    }

    #[test]
    fn breaks_come_every_few_minutes_of_airtime() {
        let shows = library("tiktok", 60, &["a", "b", "c", "d"], 25.0);
        for seed in 0..20 {
            let mut lineup = Lineup::new(Channel::TikTok, seed, bumpers());
            lineup.refresh(shows.iter().cloned());
            let slots = schedule(&mut lineup, 300);
            assert!(slots[0].show().is_some());
            let mut since = 0.0;
            let mut last: Option<String> = None;
            let mut breaks = 0;
            for slot in &slots {
                match &slot.program {
                    Program::Show(_) => since += slot.airtime,
                    Program::Break(bumper) => {
                        // At the end of the show that crosses the mark.
                        assert!(
                            (BREAK_EVERY..BREAK_EVERY + 25.0).contains(&since),
                            "seed {seed}: {since}"
                        );
                        assert_ne!(last.as_deref(), Some(bumper.name.as_str()), "seed {seed}");
                        assert!((slot.airtime - bumper.seconds).abs() < 1e-9);
                        last = Some(bumper.name.clone());
                        since = 0.0;
                        breaks += 1;
                    }
                }
            }
            assert!(breaks >= 25, "seed {seed}: {breaks}");
        }
    }

    #[test]
    fn tuning_in_can_land_in_a_break() {
        // Shows longer than BREAK_EVERY: a break after every one.
        let shows = library("youtube", 10, &["a", "b"], 300.0);
        let mut lineup = Lineup::new(Channel::YouTube, 3, bumpers());
        lineup.refresh(shows.iter().cloned());
        let slots = schedule(&mut lineup, 6);
        let on_break = slots.iter().find(|s| s.show().is_none()).unwrap().clone();
        let now = on_break.starts_at + 2.0;
        let on = lineup.slots(now, None, 2);
        assert_eq!(on[0], on_break);
        assert!(on[1].show().is_some());
    }
}

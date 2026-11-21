// The spec of tutti-shared-plugin's core, in C++. No SDK types in this file.
#pragma once
#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <chrono>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <string>
#include <thread>
#include <vector>
#ifdef _WIN32
#include <process.h>
#else
#include <unistd.h>
#endif

namespace shared {

constexpr uint32_t kLevel = 0, kAux = 1, kMode = 2, kTone = 3, kMeter = 4, kBypass = 5, kProgram = 6, kParams = 5, kModes = 5;
// kBypass is core.rs's BYPASS: listed by the VST3 shells after the meter, flagged as the bypass parameter. kProgram is core.rs's
// PROGRAM: listed after it, the program-change parameter of the unit that owns the program list.
// `tone`'s plain domain (core.rs, "Spec"): a decreasing exponential, 20000 * 1000^(-n).
constexpr double kTonePlainMax = 20000.0, kTonePlainMin = 20.0;
inline double tonePlain (double n)
{
	return kTonePlainMax * std::pow (kTonePlainMin / kTonePlainMax, std::min (1.0, std::max (0.0, n)));
}
inline double toneNormalized (double plain)
{
	double p = std::min (kTonePlainMax, std::max (kTonePlainMin, plain));
	return std::min (1.0, std::max (0.0, std::log (kTonePlainMax / p) / std::log (kTonePlainMax / kTonePlainMin)));
}
constexpr uint32_t kLatency = 137, kTail = 8191;
// The tail byte (core.rs, "Tail"): the default, the infinite marker (kInfiniteTail), or a finite 61 s.
constexpr uint32_t kTailInfinite = 1, kTailPastCap = 2, kTails = 3, kTailPastCapFrames = 48000 * 61;
// The rings nibble (core.rs, "Rings"): the high nibble of the tail byte, what the plugin does with a trigger (any non-zero input
// frame), and not what it reports. Exact, Longer, Hiss and Late; a ring is kRingLevel from its onset for its length, inclusive.
constexpr uint32_t kRingsExact = 1, kRingsLonger = 2, kRingsHiss = 3, kRingsLate = 4, kRings = 5;
constexpr uint32_t kRingFrames = 8191, kRingExtra = 4096, kLateFrames = 64;
constexpr float kRingLevel = 0.1f, kHissLevel = 0.001f;
// The fault nibble (core.rs, "Faults"): the high nibble of the mode byte; the first `process` aborts or never returns.
// kFaultRecordClose is no fault: on close the plugin writes one byte to the close record (core.rs, "Faults").
constexpr uint32_t kFaultCrash = 1, kFaultHang = 2, kFaultRecordClose = 3, kFaults = 4;
constexpr uint32_t kMagic = 0x31505354; // "TSP1"
constexpr size_t kStateHead = 24;
// A transport's flags (core.rs, "State"), and how many the log keeps.
constexpr uint32_t kPlaying = 1, kRecording = 2, kCycle = 4, kTempoValid = 8, kBeatValid = 16;
constexpr uint32_t kLogTransports = 16;
// The notice byte (core.rs, "Notice"): none, an edit on every state load, or one gesture on `aux` on every
// state load (its begin, a change to each of kGestureValues, its end).
constexpr uint32_t kNoticeOnLoad = 1, kNoticeGestureOnLoad = 2, kNotices = 3;
constexpr double kGestureValues[2] = {0.375, 0.625};

inline float tag (uint32_t bus, size_t ch) { return float (bus * 1000 + ch + 1); }

// 1 param, 2 on, 3 off, 4 control change (recorded, never acted on; this plugin's VST3 shell receives none),
// 5 data event (id: first byte, value: size), 6 null parameter list, 7 flush (core.rs, "VST3 controllers and flushes")
struct Ev { int kind; uint32_t offset; uint32_t id; double value; };

struct Core
{
	std::atomic<double> level {0.5}, aux {0.25}, tone {0.5}, bypass {0.0};
	std::atomic<uint32_t> mode {0}, notice {0}, tailSel {0}, ringsSel {0}, faultSel {0};
	std::thread::id creator = std::this_thread::get_id ();
	// A scratch Core, which parses a state and is never a plugin, does not record (`records` false).
	bool records;
	explicit Core (bool records_ = true) : records (records_) {}
	// With kFaultRecordClose selected, writes whether the closing thread is the creating one: the file and bytes of
	// transcript.rs's `close_record_path`, `CLOSED_ON_CREATOR` and `CLOSED_OFF_CREATOR`.
	~Core ()
	{
		if (!records || faultSel != kFaultRecordClose) return;
#ifdef _WIN32
		auto pid = _getpid ();
#else
		auto pid = getpid ();
#endif
		auto path = std::filesystem::temp_directory_path () / ("tutti-shared-plugin-close-" + std::to_string (pid));
		std::ofstream (path, std::ios::binary | std::ios::trunc) << (std::this_thread::get_id () == creator ? '1' : '0');
	}
	uint32_t tailSamples () const
	{
		return tailSel == kTailInfinite ? 0xFFFFFFFFu : tailSel == kTailPastCap ? kTailPastCapFrames : kTail;
	}
	std::atomic<uint64_t> calls {0}, frames {0};
	std::atomic<uint32_t> prepares {0}, maxBlock {0};
	std::atomic<double> rate {0};
	std::array<std::atomic<uint32_t>, 16> blocks {};
	std::atomic<uint32_t> blocksN {0};
	// What the host told the plugin of its automation state (core.rs, "State"): how many times, and the last raw value.
	std::atomic<uint32_t> automationN {0}, automationLast {0};
	struct Log { std::atomic<uint64_t> head {0}, id {0}; std::atomic<double> value {0}; };
	std::array<Log, 32> log;
	std::atomic<uint32_t> logN {0};
	std::atomic<uint32_t> overflow {0};
	struct Told { std::atomic<uint64_t> head {0}; std::atomic<uint32_t> flags {0}; std::atomic<double> tempo {0}, beat {0}; };
	std::array<Told, kLogTransports> told;
	std::atomic<uint32_t> toldN {0};

	// audio side
	uint32_t dspMode = 0, dspRings = 0, noise = 0x12345678; double dspLevel = 0.5; int held = -1; size_t pos = 0;
	int64_t age = -1;
	std::vector<std::vector<double>> delay;
	std::vector<Ev> events;

	bool param (uint32_t id, double& v) const
	{
		if (id == kLevel) v = level; else if (id == kAux) v = aux; else if (id == kTone) v = tone; else if (id == kBypass) v = bypass;
		else if (id == kMode) v = double (mode) / double (kModes - 1);
		else if (id == kMeter) v = 0.0; else return false;
		return true;
	}
	void setParam (uint32_t id, double v)
	{
		v = std::min (1.0, std::max (0.0, v));
		if (id == kLevel) level = v; else if (id == kAux) aux = v; else if (id == kTone) tone = v; else if (id == kBypass) bypass = v;
		else if (id == kMode) mode = std::min<uint32_t> (kModes - 1, uint32_t (std::lround (v * (kModes - 1))));
	}
	uint32_t latency () const { return mode == 1 ? kLatency : 0; }

	void prepare (double r, size_t maxB)
	{
		dspMode = mode; dspRings = ringsSel; age = -1; noise = 0x12345678; held = -1; pos = 0;
		delay.assign (8, std::vector<double> (kLatency, 0.0));
		events.clear (); events.reserve (128);
		calls = 0; frames = 0; blocksN = 0; logN = 0; toldN = 0; overflow = 0; rate = r; maxBlock = uint32_t (maxB); prepares++;
	}
	void reset ()
	{
		held = -1; age = -1;
		for (auto& l : delay) std::fill (l.begin (), l.end (), 0.0);
	}
	void push (Ev e)
	{
		record (e);
		if (events.size () < 128) events.push_back (e);
		else
			overflow = 1;
	}
	// A VST3 parameter flush (a process call with numSamples == 0): records it,
	// applies the parameter points queued since the last block at once and
	// forgets the points. No block is counted and nothing is rendered.
	void flush ()
	{
		record ({7, 0, 0, 0});
		events.erase (std::remove_if (events.begin (), events.end (), [this] (const Ev& e) {
			if (e.kind != 1) return false;
			setParam (e.id, e.value);
			return true;
		}), events.end ());
	}
	void record (Ev e)
	{
		uint32_t n = logN;
		if (n < 32)
		{
			log[n].head = (uint64_t (e.kind) << 32) | e.offset; log[n].id = e.id; log[n].value = e.value;
			logN = n + 1;
		}
		else
			overflow = 1;
	}

	// A transport the host sent with the block about to be processed, as sent.
	void logTransport (uint32_t offset, uint32_t flags, double tempo, double beat)
	{
		uint32_t n = toldN;
		if (n < kLogTransports)
		{
			told[n].head = (std::min<uint64_t> (calls, UINT32_MAX) << 32) | offset;
			told[n].flags = flags; told[n].tempo = tempo; told[n].beat = beat;
		}
		if (n < UINT32_MAX) toldN = n + 1;
	}

	// The ring behaviours' render (core.rs, "Rings"), frame by frame: the age is the frame's.
	template <typename S>
	void renderRings (size_t from, size_t to, const std::vector<std::vector<S>>& in, S* const* out, size_t nOut)
	{
		int64_t length = dspRings == kRingsLonger ? kRingFrames + kRingExtra : kRingFrames;
		int64_t onset = dspRings == kRingsLate ? kLateFrames : 0;
		float hiss = dspRings == kRingsHiss ? kHissLevel : 0.f;
		for (size_t i = from; i < to; ++i)
		{
			for (size_t c = 0; c < in.size (); ++c)
				if (double (in[c][i]) != 0.0) age = 0;
			bool ringing = age >= onset && age <= onset + length;
			for (size_t c = 0; c < nOut; ++c)
			{
				// The Rust twin's xorshift32 (core.rs): same shifts, same seed, same hiss.
				noise ^= noise << 13; noise ^= noise >> 17; noise ^= noise << 5;
				float n = (float (noise) / float (UINT32_MAX) * 2.f - 1.f) * hiss;
				out[c][i] = S ((ringing ? kRingLevel : 0.f) + n);
			}
			if (age >= 0) ++age;
		}
	}

	template <typename S>
	void render (size_t from, size_t to, uint32_t bus, const std::vector<std::vector<S>>& in,
	             S* const* out, size_t nOut)
	{
		if (dspRings != 0) { renderRings (from, to, in, out, nOut); return; }
		for (size_t c = 0; c < nOut; ++c)
			for (size_t i = from; i < to; ++i)
			{
				double x = c < in.size () ? double (in[c][i]) : 0.0;
				if (dspMode == 0) out[c][i] = S (x + tag (bus, c));
				else if (dspMode == 1)
				{
					auto& l = delay[std::min<size_t> (c, 7)];
					size_t k = (pos + i) % l.size ();
					double old = l[k]; l[k] = x; out[c][i] = S (old + tag (bus, c));
				}
				else if (dspMode == 2) out[c][i] = S (dspLevel);
				else if (dspMode == 3) out[c][i] = S (held < 0 ? 0.0 : double (held) + 1000.0 * double (c));
			}
	}

	// `in` is a copy of the input (never aliases `out`).
	template <typename S>
	void process (size_t nFrames, uint32_t bus, const std::vector<std::vector<S>>& in, S* const* out, size_t nOut)
	{
		if (faultSel == kFaultCrash) std::abort ();
		if (faultSel == kFaultHang) for (;;) std::this_thread::sleep_for (std::chrono::seconds (1));
		uint64_t n = calls++; frames += nFrames;
		if (n >= 16) overflow = 1;
		blocks[n % 16] = uint32_t (nFrames); blocksN = uint32_t (std::min<uint64_t> (n + 1, UINT32_MAX));
		dspLevel = level;
		std::stable_sort (events.begin (), events.end (), [] (const Ev& a, const Ev& b) { return a.offset < b.offset; });
		size_t at = 0;
		for (auto& e : events)
		{
			size_t o = std::min<size_t> (e.offset, nFrames);
			render (at, o, bus, in, out, nOut); at = o;
			if (e.kind == 1) { setParam (e.id, e.value); if (e.id == kLevel) dspLevel = std::min (1.0, std::max (0.0, e.value)); }
			else if (e.kind == 2) held = int (e.id);
			else if (e.kind == 3 && held == int (e.id)) held = -1;
		}
		render (at, nFrames, bus, in, out, nOut);
		pos = (pos + nFrames) % kLatency;
		events.clear ();
	}

	template <typename T> static void put (std::vector<uint8_t>& b, T v)
	{
		auto p = reinterpret_cast<const uint8_t*> (&v); b.insert (b.end (), p, p + sizeof (T));
	}
	std::vector<uint8_t> save () const
	{
		std::vector<uint8_t> b;
		put<uint32_t> (b, kMagic); put<uint32_t> (b, mode | faultSel << 4 | notice << 16 | (tailSel | ringsSel << 4) << 24); put<double> (b, level); put<double> (b, aux);
		put<uint64_t> (b, calls); put<uint64_t> (b, frames); put<uint32_t> (b, prepares);
		put<double> (b, rate); put<uint32_t> (b, maxBlock); put<uint32_t> (b, overflow);
		uint32_t nb = blocksN, kept = std::min<uint32_t> (nb, 16);
		put<uint32_t> (b, kept);
		for (uint32_t k = 0; k < kept; ++k) put<uint32_t> (b, blocks[(nb - kept + k) % 16]);
		uint32_t ne = logN; put<uint32_t> (b, ne);
		for (uint32_t k = 0; k < ne; ++k) { put<uint64_t> (b, log[k].head); put<uint64_t> (b, log[k].id); put<double> (b, log[k].value); }
		uint32_t nt = toldN; put<uint32_t> (b, nt);
		for (uint32_t k = 0; k < std::min (nt, kLogTransports); ++k)
		{
			uint64_t h = told[k].head;
			put<uint32_t> (b, uint32_t (h >> 32)); put<uint32_t> (b, uint32_t (h)); put<uint32_t> (b, told[k].flags);
			put<uint32_t> (b, 0); put<double> (b, told[k].tempo); put<double> (b, told[k].beat);
		}
		put<uint32_t> (b, automationN); put<uint32_t> (b, automationLast);
		return b;
	}
	bool load (const uint8_t* p, size_t n)
	{
		if (n < kStateHead) return false;
		uint32_t magic, word, m, editor, nt, tl, rg, fl; double l, a;
		std::memcpy (&magic, p, 4); std::memcpy (&word, p + 4, 4); std::memcpy (&l, p + 8, 8); std::memcpy (&a, p + 16, 8);
		m = word & 0xF; fl = (word >> 4) & 0xF; editor = (word >> 8) & 0xFF; nt = (word >> 16) & 0xFF; tl = (word >> 24) & 0xF; rg = word >> 28;
		// This plugin has no editor: it refuses the editor byte, and takes the notice byte.
		if (magic != kMagic || m >= kModes || editor != 0 || nt >= kNotices || tl >= kTails || rg >= kRings || fl >= kFaults || !std::isfinite (l) || !std::isfinite (a)) return false;
		mode = m; notice = nt; tailSel = tl; ringsSel = rg; faultSel = fl; setParam (kLevel, l); setParam (kAux, a);
		return true;
	}
};

} // namespace shared

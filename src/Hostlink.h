/* Hostlink.h -- host-pushed state channels + Hyprland workspace overlay
 *
 * Header-only Kaleidoscope plugin pair. Include from your sketch (.ino) once.
 *
 *   #include "Hostlink.h"
 *
 *   KALEIDOSCOPE_INIT_PLUGINS(
 *     ...,
 *     LEDControl,
 *     ...your LED effects...,
 *     Focus,               // Kaleidoscope-FocusSerial
 *     Hostlink,            // transport (Focus commands + channel registry)
 *     HostlinkWorkspaces   // overlay; must come AFTER your LED effects
 *   );
 *
 * ---------------------------------------------------------------------------
 * Wire protocol (over the Focus serial interface, newline terminated)
 *
 *   hostlink.set <channel> <len> <b0> <b1> ... <b(len-1)>
 *   hostlink.clear <channel>
 *
 * All numbers are DECIMAL (Focus parses with parseInt). Each `set` is a
 * complete snapshot of the channel's state, never a delta, so a lost frame or
 * a reconnect cannot leave things inconsistent. The keyboard replies with a
 * line `ok` or `err`, followed by Focus's usual "\r\n." terminator.
 *
 * A channel is "stale" if no `set` has arrived within its timeout (default
 * 6000 ms) or after `hostlink.clear`. Consumers must treat a stale channel as
 * "no data" and do nothing.
 *
 * Channel 0: Hyprland workspaces. 11 bytes: workspaces 1..10, then the special
 * workspace. Each byte is a bitfield, see HostlinkWorkspaces::Flags.
 *
 * To add a feature later: write another small plugin that owns a
 * HostlinkChannel with a new id and registers it in onSetup(), exactly as
 * HostlinkWorkspaces does. The transport does not change.
 * ---------------------------------------------------------------------------
 */

#pragma once

#include <Arduino.h>
#include <stdint.h>

#ifdef __AVR__
#include <avr/pgmspace.h>
#endif

#include "Kaleidoscope.h"
#include "Kaleidoscope-FocusSerial.h"
#include "Kaleidoscope-LEDControl.h"

namespace kaleidoscope {
namespace plugin {

// ---------------------------------------------------------------------------
// One host-pushed state buffer.
// ---------------------------------------------------------------------------
class HostlinkChannel {
 public:
  static constexpr uint8_t kMaxPayload = 16;

  explicit HostlinkChannel(uint8_t id, uint16_t timeout_ms = 6000)
    : id_(id), timeout_ms_(timeout_ms) {}

  uint8_t id() const {
    return id_;
  }
  uint8_t length() const {
    return len_;
  }
  const uint8_t *data() const {
    return buf_;
  }

  bool isFresh(uint32_t now_ms) const {
    return valid_ && (uint32_t)(now_ms - rx_ms_) <= timeout_ms_;
  }

  void store(const uint8_t *src, uint8_t len, uint32_t now_ms) {
    for (uint8_t i = 0; i < len; i++) buf_[i] = src[i];
    len_    = len;
    rx_ms_  = now_ms;
    valid_  = true;
  }

  void invalidate() {
    valid_ = false;
    len_   = 0;
  }

  bool hasData() const {
    return valid_;
  }
  uint32_t ageMs(uint32_t now_ms) const {
    return now_ms - rx_ms_;
  }

 private:
  uint8_t id_;
  uint16_t timeout_ms_;
  uint8_t buf_[kMaxPayload] = {0};
  uint8_t len_              = 0;
  uint32_t rx_ms_           = 0;
  bool valid_               = false;
};

// ---------------------------------------------------------------------------
// Transport plugin: Focus commands + channel registry.
// ---------------------------------------------------------------------------
class Hostlink : public kaleidoscope::Plugin {
 public:
  static constexpr uint8_t kMaxChannels = 4;

  void registerChannel(HostlinkChannel *ch) {
    if (count_ < kMaxChannels) channels_[count_++] = ch;
  }

  EventHandlerResult onNameQuery() {
    return ::Focus.sendName(F("Hostlink"));
  }

  EventHandlerResult onFocusEvent(const char *input) {
    const char *cmd_set   = PSTR("hostlink.set");
    const char *cmd_clear = PSTR("hostlink.clear");
    const char *cmd_stat  = PSTR("hostlink.status");

    if (::Focus.inputMatchesHelp(input))
      return ::Focus.printHelp(cmd_set, cmd_clear, cmd_stat);

    if (::Focus.inputMatchesCommand(input, cmd_stat)) {
      // One line per channel: id hasData fresh len age_ms
      const uint32_t now = Runtime.millisAtCycleStart();
      for (uint8_t i = 0; i < count_; i++) {
        HostlinkChannel *c = channels_[i];
        ::Focus.send(c->id(), c->hasData(), c->isFresh(now), c->length(), c->ageMs(now));
        Runtime.serialPort().println();
      }
      return EventHandlerResult::EVENT_CONSUMED;
    }

    const bool is_set = ::Focus.inputMatchesCommand(input, cmd_set);
    if (!is_set && !::Focus.inputMatchesCommand(input, cmd_clear))
      return EventHandlerResult::OK;

    uint8_t id = 0;
    ::Focus.read(id);
    HostlinkChannel *ch = find(id);
    if (ch == nullptr) return reply(false);

    if (!is_set) {
      ch->invalidate();
      return reply(true);
    }

    uint8_t len = 0;
    ::Focus.read(len);
    if (len > HostlinkChannel::kMaxPayload) return reply(false);

    // Parse into a temporary so a malformed frame never half-updates a channel.
    uint8_t tmp[HostlinkChannel::kMaxPayload];
    for (uint8_t i = 0; i < len; i++) ::Focus.read(tmp[i]);
    if (!atEndOfLine()) return reply(false);  // more bytes than declared

    ch->store(tmp, len, Runtime.millisAtCycleStart());
    return reply(true);
  }

 private:
  // True if only spaces/CR remain before the newline (or nothing is pending).
  // Never blocks, and tolerates CRLF from cooked ttys and trailing spaces.
  static bool atEndOfLine() {
    int c;
    while ((c = Runtime.serialPort().peek()) == ' ' || c == '\r')
      Runtime.serialPort().read();
    return c == '\n' || c < 0;
  }

  HostlinkChannel *find(uint8_t id) {
    for (uint8_t i = 0; i < count_; i++)
      if (channels_[i]->id() == id) return channels_[i];
    return nullptr;
  }

  EventHandlerResult reply(bool ok) {
    Runtime.serialPort().print(ok ? F("ok") : F("err"));
    return EventHandlerResult::EVENT_CONSUMED;
  }

  HostlinkChannel *channels_[kMaxChannels] = {nullptr};
  uint8_t count_                           = 0;
};

}  // namespace plugin
}  // namespace kaleidoscope

// Declared at global scope here so HostlinkWorkspaces (below) can use it.
// The definitions live at the bottom of the file.
extern kaleidoscope::plugin::Hostlink Hostlink;

namespace kaleidoscope {
namespace plugin {

// ---------------------------------------------------------------------------
// Overlay plugin: Hyprland workspaces on the number row + Tab while Super held.
// ---------------------------------------------------------------------------
class HostlinkWorkspaces : public kaleidoscope::Plugin {
 public:
  static constexpr uint8_t kChannelId = 0;
  static constexpr uint8_t kNumKeys   = 11;  // workspaces 1..10 + special

  // Per-workspace flag bits, as sent by the host daemon.
  enum Flags : uint8_t {
    CLIENTS    = 1 << 0,  // has at least one window
    FULLSCREEN = 1 << 1,  // has a fullscreen window
    URGENT     = 1 << 2,  // has an urgent window
    VISIBLE    = 1 << 3,  // shown on some monitor
    FOCUSED    = 1 << 4,  // shown on the focused monitor and not covered
  };

  // Colour rules, highest priority first (see colorFor()):
  //   urgent (flashing red) > focused (white) > fullscreen (rainbow)
  //   > visible (yellow-green) > has clients (green) > otherwise (dim blue)
  // The palette itself lives in colorFor(); tune the numbers there.

  // Urgent flash: red for the first half of each period, then the next colour
  // in line. 333 ms period = about 3 flashes per second.
  static constexpr uint16_t kFlashPeriodMs = 333;

  // Rainbow speed: hue advances by 1 (of 256) every 2^kRainbowShift ms, so one
  // full cycle takes 256 << kRainbowShift ms. 3 = ~2.0 s, 2 = ~1.0 s.
  static constexpr uint8_t kRainbowShift = 2;

  HostlinkWorkspaces() : channel_(kChannelId) {}

  EventHandlerResult onSetup() {
    ::Hostlink.registerChannel(&channel_);
    return EventHandlerResult::OK;
  }

  EventHandlerResult onFocusEvent(const char *input) {
    const char *cmd_super = PSTR("hostlink.super");
    if (::Focus.inputMatchesHelp(input)) return ::Focus.printHelp(cmd_super);
    if (::Focus.inputMatchesCommand(input, cmd_super)) {
      // super_held painted
      ::Focus.send(super_held_, painted_);
      return EventHandlerResult::EVENT_CONSUMED;
    }
    return EventHandlerResult::OK;
  }

  EventHandlerResult onKeyEvent(KeyEvent &event) {
    if (!event.addr.isValid()) return EventHandlerResult::OK;
    if (event.key != Key_LeftGui) return EventHandlerResult::OK;

    if (keyToggledOn(event.state))
      super_held_ = true;
    else if (keyToggledOff(event.state))
      super_held_ = false;
    return EventHandlerResult::OK;
  }

  EventHandlerResult beforeSyncingLeds() {
    const uint32_t now = Runtime.millisAtCycleStart();
    const bool active  = super_held_ && channel_.isFresh(now);

    if (!active) {
      // Hand the LEDs back to the active LED effect, once.
      if (painted_) {
        for (uint8_t i = 0; i < kNumKeys; i++) ::LEDControl.refreshAt(addrOf(i));
        painted_ = false;
      }
      return EventHandlerResult::OK;
    }

    const bool flash_on = (now % kFlashPeriodMs) < (kFlashPeriodMs / 2);

    uint8_t fullscreen_seen = 0;
    for (uint8_t i = 0; i < kNumKeys; i++) {
      const uint8_t flags = (i < channel_.length()) ? channel_.data()[i] : 0;

      // Each fullscreen workspace gets a fixed hue offset by its order among
      // fullscreen workspaces, whether or not it is shown as rainbow right now
      // (so its hue doesn't jump when urgent flashes or focus changes).
      uint8_t hue = 0;
      if (flags & FULLSCREEN) {
        hue = (uint8_t)((now >> kRainbowShift) + fullscreen_seen * 32);
        fullscreen_seen++;
      }

      ::LEDControl.setCrgbAt(addrOf(i), colorFor(flags, flash_on, hue));
    }
    painted_ = true;
    return EventHandlerResult::OK;
  }

 private:
  static cRGB colorFor(uint8_t flags, bool flash_on, uint8_t hue) {
    if ((flags & URGENT) && flash_on) return CRGB(255, 0, 0);       // red flash
    if (flags & FOCUSED) return CRGB(255, 255, 255);                // white
    if (flags & FULLSCREEN) return hsvToRgb(hue, 255, 255);         // rainbow
    if (flags & VISIBLE) return CRGB(200, 255, 0);                  // yellow-green
    if (flags & CLIENTS) return CRGB(0, 160, 0);                    // dark green
    return CRGB(0, 0, 96);                                          // dim blue
  }

  // Model 100 matrix positions {row, col}: keys 1..5, 6..0, then Tab.
  static KeyAddr addrOf(uint8_t i) {
    static const uint8_t kPos[kNumKeys][2] PROGMEM = {
      {0, 1}, {0, 2}, {0, 3}, {0, 4}, {0, 5},      // 1 2 3 4 5
      {0, 10}, {0, 11}, {0, 12}, {0, 13}, {0, 14},  // 6 7 8 9 0
      {1, 6},                                        // Tab -> special workspace
    };
    return KeyAddr(pgm_read_byte(&kPos[i][0]), pgm_read_byte(&kPos[i][1]));
  }

  HostlinkChannel channel_;
  bool super_held_ = false;
  bool painted_    = false;
};

}  // namespace plugin
}  // namespace kaleidoscope

kaleidoscope::plugin::Hostlink Hostlink;
kaleidoscope::plugin::HostlinkWorkspaces HostlinkWorkspaces;

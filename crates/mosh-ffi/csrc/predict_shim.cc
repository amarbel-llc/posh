// Predictor C-ABI shim: drives mosh's PredictionEngine (the predictive
// local-echo overlay) with an *injected clock* so its timing-driven behavior is
// deterministic and characterizable from Rust (task #8, ADR 0004).
//
// Builds light because of the timing.h decouple (#5): terminaloverlay.cc now
// pulls only src/network/timing.h, not the crypto/protobuf transport stack.
//
// We provide the ONLY definition of Network::timestamp() (network.cc is not
// linked), reading a process-global the test sets — so a fixed script always
// renders the same overlay. The render path mirrors OverlayManager::apply:
// cull then apply onto a copy of the confirmed framebuffer.

#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <string>

#include "src/frontend/terminaloverlay.h"
#include "src/terminal/parser.h"
#include "src/terminal/terminal.h"
#include "src/terminal/terminalframebuffer.h"

namespace {
uint64_t g_clock_ms = 0;
}

// The clock the predictor reasons about (GLITCH_THRESHOLD, send_interval, ...).
namespace Network {
uint64_t timestamp(void) { return g_clock_ms; }
uint16_t timestamp16(void) { return static_cast<uint16_t>(g_clock_ms); }
uint16_t timestamp_diff(uint16_t tsnew, uint16_t tsold) {
  return tsnew - tsold;
}
} // namespace Network

namespace {
struct MoshPredict {
  Terminal::Emulator emu;
  Parser::UTF8Parser parser;
  Parser::Actions actions;
  Overlay::PredictionEngine pred;

  MoshPredict(int w, int h) : emu(w, h), parser(), actions(), pred() {}
};

std::string render_fb(const Terminal::Framebuffer &fb) {
  std::string out;
  const Terminal::Framebuffer::rows_type &rows = fb.get_rows();
  for (size_t r = 0; r < rows.size(); ++r) {
    const Terminal::Row *row = rows[r].get();
    for (size_t c = 0; c < row->cells.size(); ++c) {
      row->cells[c].print_grapheme(out);
    }
    if (r + 1 < rows.size()) {
      out.push_back('\n');
    }
  }
  return out;
}
} // namespace

extern "C" {

// Process-global injected clock (timestamp() is a free function, not
// per-handle).
void mosh_clock_set(uint64_t ms) { g_clock_ms = ms; }

void *mosh_predict_new(int width, int height, int display_pref,
                       int predict_overwrite) {
  MoshPredict *m = new MoshPredict(width, height);
  m->pred.set_display_preference(
      static_cast<Overlay::PredictionEngine::DisplayPreference>(display_pref));
  m->pred.set_predict_overwrite(predict_overwrite != 0);
  return m;
}

void mosh_predict_free(void *h) { delete static_cast<MoshPredict *>(h); }

void mosh_predict_set_send_interval(void *h, unsigned int x) {
  static_cast<MoshPredict *>(h)->pred.set_send_interval(x);
}

void mosh_predict_set_frame_sent(void *h, uint64_t x) {
  static_cast<MoshPredict *>(h)->pred.set_local_frame_sent(x);
}

void mosh_predict_set_frame_acked(void *h, uint64_t x) {
  static_cast<MoshPredict *>(h)->pred.set_local_frame_acked(x);
}

void mosh_predict_set_frame_late_acked(void *h, uint64_t x) {
  static_cast<MoshPredict *>(h)->pred.set_local_frame_late_acked(x);
}

// Feed host/server VT bytes into the emulator (the confirmed frame predictions
// are validated against).
void mosh_predict_feed_server(void *h, const char *data, size_t len) {
  MoshPredict *m = static_cast<MoshPredict *>(h);
  for (size_t i = 0; i < len; ++i) {
    m->parser.input(data[i], m->actions);
    for (const Parser::ActionPointer &a : m->actions) {
      a->act_on_terminal(&m->emu);
    }
    m->actions.clear();
  }
  (void)m->emu.read_octets_to_host();
}

// One user keystroke byte fed to the predictor against the current frame.
void mosh_predict_key(void *h, char byte) {
  MoshPredict *m = static_cast<MoshPredict *>(h);
  m->pred.new_user_byte(byte, m->emu.get_fb());
}

// Render the displayed frame with predictions overlaid (cull then apply onto a
// copy, as OverlayManager::apply does). Caller frees via mosh_string_free.
char *mosh_predict_render(void *h) {
  MoshPredict *m = static_cast<MoshPredict *>(h);
  Terminal::Framebuffer disp = m->emu.get_fb();
  m->pred.cull(disp);
  m->pred.apply(disp);
  std::string out = render_fb(disp);
  char *buf = static_cast<char *>(std::malloc(out.size() + 1));
  std::memcpy(buf, out.data(), out.size());
  buf[out.size()] = '\0';
  return buf;
}

} // extern "C"

// ByteAsk port_check adapter for C++ strategies (harness protocol 1).
//
// Header-only, C++17, standard library only. It speaks the same line protocol as
// port_harness.py, so the port_check harness can replay one recording through the Python
// strategy and its C++ port and compare every order and decision.
//
//   #include "port_harness.hpp"
//   struct Imbalance : byteask_port::Strategy {
//     void on_md(byteask_port::Context& ctx, const byteask_port::Event& ev) override {
//       if (!ev.bids.empty() && !ev.asks.empty() && ev.bids[0].qty > 2 * ev.asks[0].qty)
//         ctx.new_order("o1", ev.sym(), 'B', ev.asks[0].px, 1, byteask_port::Tif::Ioc);
//     }
//   };
//   int main() { Imbalance s; return byteask_port::run(s); }
//
// What the harness relies on (port_check runs this version twice, the second time with new
// heap memory filled with a pattern, to catch reads of uninitialised memory):
// - Time is ctx.now() (integer nanoseconds of market time). Never read a clock.
// - Randomness starts from ctx.seed(). Never seed from time, addresses or std::random_device.
// - Prices and quantities are int64 (ticks or fixed-point units), exactly as in the Python
//   version. Do not order anything by pointer value.
// - stdout is the protocol channel: write nothing else to it. Log to stderr.
//
// Decisions are compared EXACTLY: decide_int, decide_float, decide_token correspond to a
// Python ctx.decide() with an int, a float, or a string (a Python bool is decide_int 1/0).
// diag() is the only value a tolerance may apply to. Separate names on purpose: an overloaded
// decide() would let an int silently become a double.

#pragma once

#include <charconv>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <exception>
#include <iostream>
#include <map>
#include <stdexcept>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

namespace byteask_port {

inline constexpr int kProtocolVersion = 1;

struct Level {
  int64_t px;
  int64_t qty;
};

enum class Tif { Day, Ioc };

class Strategy;
inline int run(Strategy& s);

namespace detail {

inline bool valid_token(std::string_view s) {
  if (s.empty() || s.size() > 64) return false;
  for (char c : s) {
    const bool ok = (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') ||
                    c == '.' || c == '_' || c == ':' || c == '+' || c == '-';
    if (!ok) return false;
  }
  return true;
}

inline void require_token(std::string_view s, const char* what) {
  if (!valid_token(s)) {
    throw std::invalid_argument(std::string(what) + " must be 1-64 characters from A-Z a-z 0-9 . _ : + - (got \"" +
                                std::string(s) + "\")");
  }
}

inline int64_t parse_i64(std::string_view s, std::string_view key) {
  int64_t v = 0;
  const auto r = std::from_chars(s.data(), s.data() + s.size(), v);
  if (r.ec != std::errc() || r.ptr != s.data() + s.size()) {
    throw std::runtime_error("field " + std::string(key) + "=" + std::string(s) + " is not an integer");
  }
  return v;
}

inline uint64_t parse_u64(std::string_view s, std::string_view key) {
  uint64_t v = 0;
  const auto r = std::from_chars(s.data(), s.data() + s.size(), v);
  if (r.ec != std::errc() || r.ptr != s.data() + s.size()) {
    throw std::runtime_error("field " + std::string(key) + "=" + std::string(s) + " is not an unsigned integer");
  }
  return v;
}

inline void append_i64(std::string& out, int64_t v) {
  char buf[32];
  const auto r = std::to_chars(buf, buf + sizeof buf, v);
  out.append(buf, r.ptr);
}

// The same text for the same double as Python's repr() after parsing: %.17g round-trips, and
// non-finite values use the spellings the harness and the Python adapter agree on.
inline void append_double(std::string& out, double v) {
  if (std::isnan(v)) {
    out += "nan";
  } else if (std::isinf(v)) {
    out += v > 0 ? "inf" : "-inf";
  } else {
    char buf[40];
    const int n = std::snprintf(buf, sizeof buf, "%.17g", v);
    out.append(buf, n > 0 ? static_cast<size_t>(n) : 0);
  }
}

}  // namespace detail

class Event {
 public:
  std::string type;
  int64_t step = 0;
  int64_t ts = 0;
  std::vector<std::pair<std::string, std::string>> fields;  // in line order
  std::vector<Level> bids;  // best first
  std::vector<Level> asks;  // best first

  const std::string* find(std::string_view key) const {
    for (const auto& kv : fields)
      if (kv.first == key) return &kv.second;
    return nullptr;
  }
  bool has(std::string_view key) const { return find(key) != nullptr; }
  const std::string& str(std::string_view key) const {
    const std::string* v = find(key);
    if (!v) throw std::runtime_error(type + " event has no " + std::string(key) + " field");
    return *v;
  }
  int64_t i64(std::string_view key) const { return detail::parse_i64(str(key), key); }
  int64_t i64_or(std::string_view key, int64_t fallback) const {
    const std::string* v = find(key);
    return v ? detail::parse_i64(*v, key) : fallback;
  }

  const std::string& sym() const { return str("sym"); }
  const std::string& kind() const { return str("kind"); }
  const std::string& id() const { return str("id"); }
  char side() const { return str("side").empty() ? '?' : str("side")[0]; }
  int64_t px() const { return i64("px"); }
  int64_t qty() const { return i64("qty"); }
  int64_t leaves() const { return i64("leaves"); }
  const std::string& reason() const { return str("reason"); }
  const std::string& tag() const { return str("tag"); }
  const std::string& liq() const { return str("liq"); }
  bool has_rec() const { return has("rec"); }
  int64_t rec() const { return i64("rec"); }

  static Event parse(const std::string& line) {
    Event ev;
    size_t pos = 0;
    bool first = true;
    while (pos <= line.size()) {
      size_t end = line.find(' ', pos);
      if (end == std::string::npos) end = line.size();
      std::string_view tok(line.data() + pos, end - pos);
      if (first) {
        ev.type = std::string(tok);
        first = false;
      } else if (!tok.empty()) {
        const size_t eq = tok.find('=');
        if (eq == std::string_view::npos) throw std::runtime_error("bad field \"" + std::string(tok) + "\"");
        ev.fields.emplace_back(std::string(tok.substr(0, eq)), std::string(tok.substr(eq + 1)));
      }
      pos = end + 1;
    }
    ev.step = ev.i64("step");
    ev.ts = ev.i64("ts");
    for (int side = 0; side < 2; ++side) {
      const char p = side == 0 ? 'b' : 'a';
      for (int i = 1;; ++i) {
        const std::string pk = std::string(1, p) + "p" + std::to_string(i);
        const std::string qk = std::string(1, p) + "q" + std::to_string(i);
        if (!ev.has(pk)) break;
        (side == 0 ? ev.bids : ev.asks).push_back(Level{ev.i64(pk), ev.i64(qk)});
      }
    }
    return ev;
  }
};

class Context {
 public:
  int64_t now() const { return now_; }
  int64_t step() const { return step_; }
  uint64_t seed() const { return seed_; }
  int depth() const { return depth_; }
  const std::vector<std::string>& symbols() const { return symbols_; }
  const std::map<std::string, std::string>& params() const { return params_; }
  std::string param(const std::string& name, const std::string& fallback) const {
    const auto it = params_.find(name);
    return it == params_.end() ? fallback : it->second;
  }

  void new_order(std::string_view id, std::string_view sym, char side, int64_t px, int64_t qty,
                 Tif tif = Tif::Day) {
    detail::require_token(id, "order id");
    detail::require_token(sym, "symbol");
    if (side != 'B' && side != 'S') throw std::invalid_argument("side must be 'B' or 'S'");
    out_ += "new id=";
    out_ += id;
    out_ += " sym=";
    out_ += sym;
    out_ += " side=";
    out_ += side;
    out_ += " px=";
    detail::append_i64(out_, px);
    out_ += " qty=";
    detail::append_i64(out_, qty);
    out_ += tif == Tif::Ioc ? " tif=ioc\n" : " tif=day\n";
  }

  void cancel(std::string_view id) {
    detail::require_token(id, "order id");
    out_ += "cancel id=";
    out_ += id;
    out_ += '\n';
  }

  void set_timer(int64_t at, std::string_view tag) {
    detail::require_token(tag, "timer tag");
    out_ += "timer at=";
    detail::append_i64(out_, at);
    out_ += " tag=";
    out_ += tag;
    out_ += '\n';
  }

  void decide_int(std::string_view name, int64_t value) {
    begin("decide", name);
    detail::append_i64(out_, value);
    out_ += '\n';
  }
  void decide_float(std::string_view name, double value) {
    begin("decide", name);
    detail::append_double(out_, value);
    out_ += '\n';
  }
  void decide_token(std::string_view name, std::string_view value) {
    detail::require_token(value, "decision value");
    begin("decide", name);
    out_ += value;
    out_ += '\n';
  }
  void diag(std::string_view name, double value) {
    begin("diag", name);
    detail::append_double(out_, value);
    out_ += '\n';
  }

 private:
  void begin(const char* verb, std::string_view name) {
    detail::require_token(name, "name");
    out_ += verb;
    out_ += ' ';
    out_ += name;
    out_ += '=';
  }

  friend int run(Strategy&);
  int64_t now_ = 0;
  int64_t step_ = 0;
  uint64_t seed_ = 0;
  int depth_ = 1;
  std::vector<std::string> symbols_;
  std::map<std::string, std::string> params_;
  std::string out_;
};

class Strategy {
 public:
  virtual ~Strategy() = default;
  virtual void on_init(Context&, const Event&) {}
  virtual void on_md(Context&, const Event&) {}
  virtual void on_ack(Context&, const Event&) {}
  virtual void on_fill(Context&, const Event&) {}
  virtual void on_reject(Context&, const Event&) {}
  virtual void on_cancelled(Context&, const Event&) {}
  virtual void on_cancel_reject(Context&, const Event&) {}
  virtual void on_timer(Context&, const Event&) {}
  virtual void on_end(Context&, const Event&) {}
};

// Answer harness events until the input ends. Returns 0 then; on an error (an exception from
// the strategy or a malformed event) prints it to stderr and returns 2, so the harness reports
// the step where it happened.
inline int run(Strategy& s) {
  std::ios::sync_with_stdio(false);
  std::cin.tie(nullptr);
  Context ctx;
  std::string line;
  try {
    while (std::getline(std::cin, line)) {
      if (!line.empty() && line.back() == '\r') line.pop_back();
      if (line.empty()) continue;
      const Event ev = Event::parse(line);
      ctx.now_ = ev.ts;
      ctx.step_ = ev.step;
      ctx.out_.clear();
      if (ev.type == "init") {
        ctx.seed_ = detail::parse_u64(ev.str("seed"), "seed");
        ctx.depth_ = static_cast<int>(ev.i64_or("depth", 1));
        ctx.symbols_.clear();
        const std::string* syms = ev.find("syms");
        if (syms && !syms->empty()) {
          size_t p = 0;
          while (p <= syms->size()) {
            size_t e = syms->find(',', p);
            if (e == std::string::npos) e = syms->size();
            ctx.symbols_.emplace_back(syms->substr(p, e - p));
            p = e + 1;
          }
        }
        ctx.params_.clear();
        for (const auto& kv : ev.fields)
          if (kv.first.size() > 2 && kv.first.compare(0, 2, "p.") == 0) ctx.params_[kv.first.substr(2)] = kv.second;
        s.on_init(ctx, ev);
      } else if (ev.type == "md") {
        s.on_md(ctx, ev);
      } else if (ev.type == "ack") {
        s.on_ack(ctx, ev);
      } else if (ev.type == "fill") {
        s.on_fill(ctx, ev);
      } else if (ev.type == "reject") {
        s.on_reject(ctx, ev);
      } else if (ev.type == "cancelled") {
        s.on_cancelled(ctx, ev);
      } else if (ev.type == "cancel_reject") {
        s.on_cancel_reject(ctx, ev);
      } else if (ev.type == "timer") {
        s.on_timer(ctx, ev);
      } else if (ev.type == "end") {
        s.on_end(ctx, ev);
      } else {
        throw std::runtime_error("unknown event type \"" + ev.type + "\" (adapter is protocol 1)");
      }
      ctx.out_ += "done\n";
      std::fwrite(ctx.out_.data(), 1, ctx.out_.size(), stdout);
      std::fflush(stdout);
    }
  } catch (const std::exception& e) {
    std::fprintf(stderr, "port_harness: %s\n", e.what());
    return 2;
  }
  return 0;
}

}  // namespace byteask_port

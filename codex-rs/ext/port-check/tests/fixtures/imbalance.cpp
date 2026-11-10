// C++ port of imbalance.py for the port_check tests. Planted defects, chosen at compile time:
//   -DPLANT_QTY_AT=N     the Nth IOC order sends one share more
//   -DPLANT_DIAG_ULP     "fair" is one representable double higher
//   -DPLANT_UNINIT       reads fresh heap memory it never initialised (0 unless the heap is
//                        filled with a pattern, as the harness's second C++ run does)
//   -DPLANT_CRASH_AT=N   aborts while answering step N
//   -DPLANT_HANG_AT=N    stops answering at step N
#include "port_harness.hpp"

#include <chrono>
#include <cmath>
#include <cstdlib>
#include <map>
#include <string>
#include <thread>
#include <utility>
#include <vector>

using byteask_port::Context;
using byteask_port::Event;
using byteask_port::Tif;

struct Imbalance : byteask_port::Strategy {
  uint64_t state = 0;
  int64_t n = 0;
  int64_t ioc = 0;
  std::map<std::string, std::pair<std::string, char>> orders;
  std::vector<std::string> quotes;
  std::map<std::string, int64_t> position;
  int64_t max_pos = 30, every = 7, timer_ns = 400000, beats = 0, md = 0, bias = 0;
  std::string variant;

  uint64_t rand() {
    uint64_t x = state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    state = x;
    return x;
  }

  std::string send(Context& ctx, const std::string& sym, char side, int64_t px, int64_t qty, Tif tif) {
    ++n;
    std::string id = "o" + std::to_string(n);
    orders[id] = {sym, side};
#ifdef PLANT_QTY_AT
    if (tif == Tif::Ioc && ++ioc == PLANT_QTY_AT) qty += 1;
#endif
    ctx.new_order(id, sym, side, px, qty, tif);
    return id;
  }

  void check_plants(Context& ctx) {
    (void)ctx;
#ifdef PLANT_CRASH_AT
    if (ctx.step() == PLANT_CRASH_AT) std::abort();
#endif
#ifdef PLANT_HANG_AT
    if (ctx.step() == PLANT_HANG_AT) std::this_thread::sleep_for(std::chrono::seconds(60));
#endif
  }

  void on_init(Context& ctx, const Event&) override {
    check_plants(ctx);
    variant = ctx.param("variant", "");
    state = ctx.seed() ? ctx.seed() : 88172645463325252ULL;
    for (const auto& s : ctx.symbols()) position[s] = 0;
    max_pos = std::stoll(ctx.param("max_pos", "30"));
    every = std::stoll(ctx.param("quote_every", "7"));
    timer_ns = std::stoll(ctx.param("timer_ns", "400000"));
#ifdef PLANT_UNINIT
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wmaybe-uninitialized"
    // The planted bug: memory used before anything was written to it.
    auto* scratch = static_cast<unsigned char*>(std::malloc(3000));
    bias = scratch[1500] != 0 ? 1 : 0;
#pragma GCC diagnostic pop
#endif
    ctx.set_timer(ctx.now() + timer_ns, "hb");
  }

  void on_md(Context& ctx, const Event& ev) override {
    check_plants(ctx);
    ++md;
    if (ev.bids.empty() || ev.asks.empty()) return;
    const int64_t bp = ev.bids[0].px, bq = ev.bids[0].qty, ap = ev.asks[0].px, aq = ev.asks[0].qty;
    double fair = static_cast<double>(bp * aq + ap * bq) / static_cast<double>(bq + aq);
#ifdef PLANT_DIAG_ULP
    fair = std::nextafter(fair, INFINITY);
#endif
    ctx.diag("fair", fair);
    ctx.diag("spread", static_cast<double>(ap - bp));
    const int64_t imb = bq - aq;
    ctx.decide_int("imb", imb > 0 ? 1 : (imb < 0 ? -1 : 0));
    if (variant == "no_orders") return;
    const int64_t qty = 1 + static_cast<int64_t>(rand() % 3) + bias;
    const int64_t pos = position[ev.sym()];
    if (bq > 2 * aq && pos + qty <= max_pos) {
      send(ctx, ev.sym(), 'B', ap, qty, Tif::Ioc);
    } else if (aq > 2 * bq && pos - qty >= -max_pos) {
      send(ctx, ev.sym(), 'S', bp, qty, Tif::Ioc);
    } else if (md % every == 0) {
      for (const auto& id : quotes) ctx.cancel(id);
      quotes.clear();
      const char side = rand() % 2 == 0 ? 'B' : 'S';
      const int64_t px = side == 'B' ? bp : ap;
      quotes.push_back(send(ctx, ev.sym(), side, px, 2, Tif::Day));
    }
  }

  void on_ack(Context& ctx, const Event&) override { check_plants(ctx); }

  void on_cancel_reject(Context& ctx, const Event&) override { check_plants(ctx); }

  void on_fill(Context& ctx, const Event& ev) override {
    check_plants(ctx);
    const auto& o = orders.at(ev.id());
    position[o.first] += o.second == 'B' ? ev.qty() : -ev.qty();
    ctx.decide_int("pos", position[o.first]);
    if (ev.leaves() == 0) {
      for (auto it = quotes.begin(); it != quotes.end(); ++it) {
        if (*it == ev.id()) {
          quotes.erase(it);
          break;
        }
      }
    }
  }

  void on_reject(Context& ctx, const Event& ev) override {
    check_plants(ctx);
    ctx.decide_token("rej", ev.reason());
    for (auto it = quotes.begin(); it != quotes.end(); ++it) {
      if (*it == ev.id()) {
        quotes.erase(it);
        break;
      }
    }
  }

  void on_cancelled(Context& ctx, const Event& ev) override {
    check_plants(ctx);
    ctx.decide_int("cxl", ev.qty());
  }

  void on_timer(Context& ctx, const Event&) override {
    check_plants(ctx);
    ++beats;
    ctx.decide_int("beat", beats);
    ctx.set_timer(ctx.now() + timer_ns, "hb");
  }

  void on_end(Context& ctx, const Event&) override {
    check_plants(ctx);
    int64_t total = 0;
    for (const auto& p : position) total += p.second;
    ctx.decide_int("final", total);
  }
};

int main() {
  Imbalance s;
  return byteask_port::run(s);
}

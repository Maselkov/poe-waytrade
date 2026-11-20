#include "ui.h"
#include "assets.h"
#include "icons.h"
#include "layer.h"
#include "price_check.h"
#include "rml_renderer.h"
#include "settings_page.h"
#include "version.h"

#include <RmlUi/Core.h>

#include <sys/stat.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <fstream>
#include <map>
#include <sstream>

namespace {

// Listing previews: an inventory cell of the item's image (dp; the image's
// own pixels).
constexpr float kPreviewCell = 47;

const char* const kPriceDoc = "ui/price_check.rml";
const char* const kMapDoc = "ui/map_check.rml";
const char* const kDebugDoc = "ui/debug.rml";
const char* const kSettingsDoc = "ui/settings.rml";
const char* const kToastDoc = "ui/toast.rml";
const char* const kDocuments[] = {kPriceDoc, kMapDoc, kSettingsDoc, kToastDoc, kDebugDoc};

// The start-up notice: shown once the daemon has answered (or after a
// moment without it), for a few seconds, fading out.
constexpr double kToastWait = 3.0, kToastShow = 6.0, kToastIn = 0.45, kToastFade = 0.5;

// ---- RmlUi interfaces ------------------------------------------------------

class System final : public Rml::SystemInterface {
public:
    double GetElapsedTime() override {
        return std::chrono::duration<double>(std::chrono::steady_clock::now() - start_).count();
    }
    bool LogMessage(Rml::Log::Type type, const Rml::String& message) override {
        if (type <= Rml::Log::LT_WARNING) poe_log("ui: %s", message.c_str());
        return true;
    }
    void SetMouseCursor(const Rml::String& name) override { cursor = name; }

    Rml::String cursor;

private:
    std::chrono::steady_clock::time_point start_ = std::chrono::steady_clock::now();
};

// RCSS has no variables, so theme files declare `@define $name value;` and
// we substitute them when the file is read.
std::string substitute_defines(const std::string& src) {
    std::vector<std::pair<std::string, std::string>> defines;
    std::string out;
    std::istringstream lines(src);
    for (std::string line; std::getline(lines, line);) {
        size_t start = line.find_first_not_of(" \t");
        if (start != std::string::npos && line.compare(start, 8, "@define ") == 0) {
            std::istringstream def(line.substr(start + 8));
            std::string name, value;
            def >> name;
            std::getline(def, value);
            value = value.substr(value.find_first_not_of(" \t") == std::string::npos ? value.size()
                                                                                        : value.find_first_not_of(" \t"));
            while (!value.empty() && (value.back() == ';' || value.back() == ' ' || value.back() == '\r'))
                value.pop_back();
            defines.emplace_back(name, value);
            continue;
        }
        out += line + "\n";
    }
    // Longest names first, so $text doesn't eat part of $text-strong.
    std::sort(defines.begin(), defines.end(), [](auto& a, auto& b) { return a.first.size() > b.first.size(); });
    for (auto& [name, value] : defines)
        for (size_t pos = 0; (pos = out.find(name, pos)) != std::string::npos; pos += value.size())
            out.replace(pos, name.size(), value);
    return out;
}

// Serves assets from POE_WAYTRADE_UI_DIR when set (for live editing),
// otherwise from the copies built into the layer.
class Files final : public Rml::FileInterface {
public:
    Files() {
        if (const char* dir = getenv("POE_WAYTRADE_UI_DIR")) dev_dir = dir;
    }

    std::string dev_dir;

    bool read(const std::string& path, std::string* out) const {
        if (!dev_dir.empty()) {
            std::ifstream f(dev_dir + "/" + path, std::ios::binary);
            if (f) {
                *out = std::string(std::istreambuf_iterator<char>(f), {});
                return true;
            }
        }
        for (size_t i = 0; i < embedded_file_count; i++) {
            if (path == embedded_files[i].path) {
                out->assign((const char*)embedded_files[i].data, embedded_files[i].size);
                return true;
            }
        }
        return false;
    }

    Rml::FileHandle Open(const Rml::String& path) override {
        auto* f = new Open_;
        if (!read(path, &f->data)) {
            delete f;
            return 0;
        }
        if (path.size() > 5 && path.compare(path.size() - 5, 5, ".rcss") == 0) f->data = substitute_defines(f->data);
        return reinterpret_cast<Rml::FileHandle>(f);
    }
    void Close(Rml::FileHandle h) override { delete reinterpret_cast<Open_*>(h); }
    size_t Read(void* buffer, size_t size, Rml::FileHandle h) override {
        auto* f = reinterpret_cast<Open_*>(h);
        size = std::min(size, f->data.size() - f->pos);
        memcpy(buffer, f->data.data() + f->pos, size);
        f->pos += size;
        return size;
    }
    bool Seek(Rml::FileHandle h, long offset, int origin) override {
        auto* f = reinterpret_cast<Open_*>(h);
        long base = origin == SEEK_SET ? 0 : origin == SEEK_CUR ? (long)f->pos : (long)f->data.size();
        long pos = base + offset;
        if (pos < 0 || pos > (long)f->data.size()) return false;
        f->pos = pos;
        return true;
    }
    size_t Tell(Rml::FileHandle h) override { return reinterpret_cast<Open_*>(h)->pos; }

private:
    struct Open_ {
        std::string data;
        size_t pos = 0;
    };
};

// ---- the data model ---------------------------------------------------------

struct UiPrice {
    Rml::String label, main, sub;
    Rml::String icon;  // currency id, once its icon has arrived
};
struct UiFilter {
    int index = 0;
    bool enabled = false;
    Rml::String label, min;
    Rml::String bounds;            // "≤ 220", for a filter with only a maximum
    // A filter with a minimum shows its bounds as two halves, each clicked
    // to type into (PriceCheck::edit_bounds) and scrolled to step: the
    // minimum, and the maximum ("max" in faint text when there's none).
    bool split = false;
    Rml::String lo, hi;
    bool hi_empty = false;
    int edit_field = -1;  // the half being typed into; -1: none
    float min_pct = 0, max_pct = 100;  // handle positions on the slider
    float roll_pct = -1;               // the item's own value; < 0: none
    bool has_slider = false, max_roll = false;
    Rml::String tier;     // "T1"; empty: none
    Rml::String section;  // weapon | base | armour | sockets | total | enchant | implicit | exarch | eater | mod | fractured | crafted | map | shaper, elder, ... (influence)
};
struct UiGroup {
    Rml::String label;
    Rml::Vector<UiFilter> filters;
};
struct UiPreviewLine {
    Rml::String kind, text;  // kind: enchant, implicit, explicit, ..., divider
    Rml::String tier;        // "T1"; empty: none
};
// A socket or link drawn over a listed item's image: a part ("x y w h", in
// pixels) of the socket sprite sheet, placed in dp from the image's top left.
struct UiSprite {
    float x = 0, y = 0, w = 0, h = 0;
    Rml::String rect;
};
struct UiListing {
    Rml::String price, sub, age, count, icon;
    Rml::String tag;  // a gem's level and quality
    // Hover preview of the first listed item.
    bool has_preview = false;
    Rml::String rarity, name, base, meta, more;
    Rml::String item_icon;  // icons.h id, once the image has arrived
    float icon_w = 0, icon_h = 0;  // dp
    Rml::Vector<UiSprite> sockets;  // then the links, drawn over them
    Rml::Vector<UiPreviewLine> lines;
};

struct UiMapStat {
    Rml::String label, value;
};
struct UiMapMod {
    int index = 0;      // into the reply's mods, for cycle_mod
    Rml::String text;
    Rml::String mark;   // deadly | warning | good | "" (CSS class)
    Rml::String tag;    // Deadly | Warning | Good | ""
};

// Settings page
struct UiHotkey {
    int index = 0;  // SettingsPage::Action
    Rml::String label, desc, key;
    Rml::String old;    // the key it had, for the warning's "Keep"
    Rml::String state;  // capturing | warned | off | ""
    bool can_clear = false;
};
struct UiAbout {
    Rml::String label, value;
};

struct Model {
    Rml::String name, base, subtitle, rarity;
    Rml::String state;  // status | prices | trade
    Rml::String status, source, thin, total, hint;
    bool status_error = false, searching = false, has_url = false, closable = false;
    // Trade search options
    Rml::String base_label, category_label, corrupted, corrupted_label;
    bool base_preset = false, preset_base = false;  // offered / active
    bool base_only = false, searched = false, stale = false;
    Rml::Vector<UiPrice> prices;
    Rml::Vector<UiGroup> filter_groups;
    Rml::Vector<UiListing> listings;
    // Price summary (the low end of the fetched listings)
    bool has_summary = false, low_value = false;
    Rml::String summary_price, summary_icon, summary_sub, summary_range;
    // A unique's poe.ninja price, shown big until a trade search prices it
    Rml::String ninja_icon, ninja_meta;
    Rml::Vector<Rml::String> debug_lines;
    // Map check
    Rml::String verdict, verdict_kind;  // kind: deadly | warning | clean
    Rml::Vector<UiMapStat> map_stats;
    Rml::Vector<UiMapMod> map_mods;
    int profile = 1;
    // Map-like items switch between the map check and the price
    bool can_switch = false;
    Rml::String map_price, map_price_label;  // poe.ninja, in the map check
    // Settings page (s_*)
    int s_tab = 0;
    Rml::Vector<Rml::String> s_tabs, s_profiles;
    Rml::String s_version;
    Rml::Vector<UiHotkey> s_hotkeys;
    int s_warning = -1;  // the hotkey row with the "reaches the game" warning
    Rml::String s_warning_title, s_warning_text, s_warning_key;
    Rml::String s_league, s_league_pinned, s_status, s_min, s_scale;
    bool s_scale_env = false;
    bool s_stash_scroll = true;
    int s_profile = 1;
    bool s_reset_armed = false;
    Rml::Vector<UiAbout> s_about;
    Rml::String s_note;
    bool s_note_error = false;
    // Start-up notice
    Rml::String t_state, t_hint, t_warn;
    bool t_busy = false;
};

Rml::String format(const char* fmt, double v) {
    char buf[64];
    snprintf(buf, sizeof(buf), fmt, v);
    return buf;
}

Rml::String chaos_text(double chaos) { return format(chaos < 10 ? "%.1fc" : "%.0fc", chaos); }

// "2184" -> "2,184"
Rml::String thousands(const std::string& digits) {
    if (digits.empty() || digits.find_first_not_of("0123456789") != std::string::npos) return digits;
    std::string out;
    int n = 0;
    for (auto it = digits.rbegin(); it != digits.rend(); ++it, ++n) {
        if (n && n % 3 == 0) out.insert(out.begin(), ',');
        out.insert(out.begin(), *it);
    }
    return out;
}

int count_listings(const PriceReply& reply) {
    int n = 0;
    for (const TradeListing& l : reply.listings) n += l.count;
    return n;
}

Rml::String rarity_class(const std::string& rarity) {
    if (rarity == "Divination Card") return "card";
    Rml::String r = rarity;
    std::transform(r.begin(), r.end(), r.begin(), ::tolower);
    return r;
}

time_t mtime(const std::string& path) {
    struct stat st;
    return stat(path.c_str(), &st) == 0 ? st.st_mtime : 0;
}

}  // namespace

struct OverlayUi::Impl {
    System system;
    Files files;
    Rml::Context* context = nullptr;
    std::map<std::string, Rml::ElementDocument*> docs;
    Model model;
    Rml::DataModelHandle handle;
    PriceCheck* price_check = nullptr;  // during update(), for event callbacks
    uint64_t revision = UINT64_MAX;
    bool range_max_handle = false;  // which handle the current slider drag moves

    void range_input(int index, Rml::Event& ev, bool press) {
        if (!price_check || !price_check->reply()) return;
        const TradeFilter* filter = nullptr;
        for (const TradeFilter& f : price_check->reply()->filters)
            if (f.index == index) filter = &f;
        if (!filter || filter->hi <= filter->lo) return;
        // An unticked row's strip only shows the range: pressing it ticks
        // the row, like clicking its text.
        if (!filter->enabled) {
            if (press) price_check->toggle_filter(index);
            return;
        }
        Rml::Element* track = ev.GetCurrentElement();
        float left = track->GetAbsoluteOffset(Rml::BoxArea::Content).x;
        float width = track->GetBox().GetSize(Rml::BoxArea::Content).x;
        if (width <= 0) return;
        float x = ev.GetParameter<float>("mouse_x", left);
        double lo = filter->lo, hi = filter->hi, step = filter->step > 0 ? filter->step : 1;
        double v = lo + std::clamp((x - left) / width, 0.0f, 1.0f) * (hi - lo);
        v = std::clamp(std::round(v / step) * step, lo, hi);
        double min = filter->min.empty() ? lo : atof(filter->min.c_str());
        double max = filter->max.empty() ? -1 : atof(filter->max.c_str());
        double max_at = max < 0 ? hi : max;
        if (press) {
            double to_min = std::abs(v - std::min(min, hi)), to_max = std::abs(v - max_at);
            range_max_handle = to_max < to_min || (to_max == to_min && v > max_at - 1e-9 && v >= min);
        }
        if (range_max_handle)
            max = v >= hi ? -1 : std::max(v, std::min(min, hi));
        else
            min = std::min(v, max_at);
        price_check->set_bounds(index, min, max);
    }
    // field 0 steps the minimum, 1 the maximum. As on the slider, a maximum
    // at the top of the range is none.
    void bounds_scroll(int index, int field, float delta) {
        if (!price_check || !price_check->reply() || delta == 0) return;
        for (const TradeFilter& f : price_check->reply()->filters) {
            if (f.index != index || f.hi <= f.lo || f.min.empty()) continue;
            double step = f.step > 0 ? f.step : 1;
            double by = delta < 0 ? step : -step;
            double min = atof(f.min.c_str());
            double max = f.max.empty() ? -1 : atof(f.max.c_str());
            if (field == 1) {
                max = std::clamp((max < 0 ? f.hi : max) + by, std::min(min, f.hi), f.hi);
                if (max >= f.hi) max = -1;
            } else {
                min = std::clamp(min + by, f.lo, max < 0 ? f.hi : max);
            }
            price_check->set_bounds(index, min, max);
        }
    }
    uint64_t placed_check = 0;
    bool debug = getenv("POE_WAYTRADE_DEBUG") != nullptr;
    double toast_started = -1;  // elapsed seconds; < 0: not yet
    bool toast_off = getenv("POE_WAYTRADE_DEMO_ITEM") || getenv("POE_WAYTRADE_DEMO_SETTINGS");
    void update_toast(PriceCheck& pc, VkExtent2D extent);
    float ui_scale = 1.0f;     // POE_WAYTRADE_UI_SCALE, else the setting
    bool ui_scale_env = false;
    void sync_settings(PriceCheck& pc, Model& m);
    int frame = 0;
    time_t loaded_mtime = 0;
    bool ok = false;

    bool init(RmlRenderer* renderer);
    void bind_model();
    void load_documents();
    time_t dev_files_mtime() const;
    void sync(PriceCheck& pc);
    void sync_debug(WaylandInput* input, VkExtent2D extent);
    void place(PriceCheck& pc, VkExtent2D extent);
    bool keep_on_screen(const char* path, VkExtent2D extent);
    void mark_cut_mods();
    void sync_map(const PriceReply& reply, uint64_t check_id, Model& m);
    void fill_prices(const PriceReply& reply, Model& m);
    void fill_preview(const TradeListing& l, UiListing& u);
    void place_sockets(const Rml::String& text, UiListing& u);
    // Map mod display order, fixed when a check opens so rows don't jump
    // around while you mark them.
    uint64_t map_order_check = 0;
    int map_order_profile = 0;
    std::vector<int> map_order;
    // The panel document for the current check.
    const char* panel_doc(const PriceCheck& pc) const {
        return pc.purpose() == PriceCheck::Purpose::Map ? kMapDoc : kPriceDoc;
    }
    const char* shown_panel = kPriceDoc;
};

OverlayUi::OverlayUi(RmlRenderer* renderer) : impl_(std::make_unique<Impl>()) {
    impl_->ok = impl_->init(renderer);
}

OverlayUi::~OverlayUi() {
    if (impl_->ok) Rml::Shutdown();
}

bool OverlayUi::ok() const { return impl_->ok; }

bool OverlayUi::Impl::init(RmlRenderer* renderer) {
    if (const char* s = getenv("POE_WAYTRADE_UI_SCALE")) ui_scale = std::max(0.25f, (float)atof(s)), ui_scale_env = true;
    Rml::SetRenderInterface(renderer);
    Rml::SetSystemInterface(&system);
    Rml::SetFileInterface(&files);
    if (!Rml::Initialise()) return false;

    const struct {
        const char* path;
        const char* family;
        Rml::Style::FontWeight weight;
    } fonts[] = {
        {"fonts/Inter-Regular.ttf", "Inter", Rml::Style::FontWeight::Normal},
        {"fonts/Inter-Medium.ttf", "Inter", Rml::Style::FontWeight(500)},
        {"fonts/Cinzel.ttf", "Cinzel", Rml::Style::FontWeight::Normal},
    };
    for (auto& f : fonts) {
        for (size_t i = 0; i < embedded_file_count; i++) {
            if (strcmp(embedded_files[i].path, f.path) != 0) continue;
            Rml::LoadFontFace({(const Rml::byte*)embedded_files[i].data, embedded_files[i].size}, f.family,
                              Rml::Style::FontStyle::Normal, f.weight, false);
        }
    }

    // The panels' settings button.
    for (size_t i = 0; i < embedded_file_count; i++)
        if (strcmp(embedded_files[i].path, "ui/gear.png") == 0)
            icons_put("gear", std::string((const char*)embedded_files[i].data, embedded_files[i].size));

    context = Rml::CreateContext("overlay", Rml::Vector2i(1920, 1080));
    if (!context) return false;
    bind_model();
    load_documents();
    return !docs.empty();
}

void OverlayUi::Impl::bind_model() {
    Rml::DataModelConstructor c = context->CreateDataModel("ui");
    if (auto h = c.RegisterStruct<UiPrice>()) {
        h.RegisterMember("label", &UiPrice::label);
        h.RegisterMember("main", &UiPrice::main);
        h.RegisterMember("sub", &UiPrice::sub);
        h.RegisterMember("icon", &UiPrice::icon);
    }
    if (auto h = c.RegisterStruct<UiFilter>()) {
        h.RegisterMember("index", &UiFilter::index);
        h.RegisterMember("enabled", &UiFilter::enabled);
        h.RegisterMember("label", &UiFilter::label);
        h.RegisterMember("min", &UiFilter::min);
        h.RegisterMember("bounds", &UiFilter::bounds);
        h.RegisterMember("split", &UiFilter::split);
        h.RegisterMember("lo", &UiFilter::lo);
        h.RegisterMember("hi", &UiFilter::hi);
        h.RegisterMember("hi_empty", &UiFilter::hi_empty);
        h.RegisterMember("edit_field", &UiFilter::edit_field);
        h.RegisterMember("min_pct", &UiFilter::min_pct);
        h.RegisterMember("roll_pct", &UiFilter::roll_pct);
        h.RegisterMember("max_pct", &UiFilter::max_pct);
        h.RegisterMember("has_slider", &UiFilter::has_slider);
        h.RegisterMember("max_roll", &UiFilter::max_roll);
        h.RegisterMember("tier", &UiFilter::tier);
        h.RegisterMember("section", &UiFilter::section);
    }
    if (auto h = c.RegisterStruct<UiPreviewLine>()) {
        h.RegisterMember("kind", &UiPreviewLine::kind);
        h.RegisterMember("text", &UiPreviewLine::text);
        h.RegisterMember("tier", &UiPreviewLine::tier);
    }
    c.RegisterArray<Rml::Vector<UiPreviewLine>>();
    if (auto h = c.RegisterStruct<UiSprite>()) {
        h.RegisterMember("x", &UiSprite::x);
        h.RegisterMember("y", &UiSprite::y);
        h.RegisterMember("w", &UiSprite::w);
        h.RegisterMember("h", &UiSprite::h);
        h.RegisterMember("rect", &UiSprite::rect);
    }
    c.RegisterArray<Rml::Vector<UiSprite>>();
    if (auto h = c.RegisterStruct<UiListing>()) {
        h.RegisterMember("sockets", &UiListing::sockets);
        h.RegisterMember("has_preview", &UiListing::has_preview);
        h.RegisterMember("rarity", &UiListing::rarity);
        h.RegisterMember("name", &UiListing::name);
        h.RegisterMember("base", &UiListing::base);
        h.RegisterMember("meta", &UiListing::meta);
        h.RegisterMember("more", &UiListing::more);
        h.RegisterMember("item_icon", &UiListing::item_icon);
        h.RegisterMember("icon_w", &UiListing::icon_w);
        h.RegisterMember("icon_h", &UiListing::icon_h);
        h.RegisterMember("lines", &UiListing::lines);
        h.RegisterMember("price", &UiListing::price);
        h.RegisterMember("sub", &UiListing::sub);
        h.RegisterMember("age", &UiListing::age);
        h.RegisterMember("count", &UiListing::count);
        h.RegisterMember("icon", &UiListing::icon);
        h.RegisterMember("tag", &UiListing::tag);
    }
    c.RegisterArray<Rml::Vector<UiPrice>>();
    c.RegisterArray<Rml::Vector<UiFilter>>();
    if (auto h = c.RegisterStruct<UiGroup>()) {
        h.RegisterMember("label", &UiGroup::label);
        h.RegisterMember("filters", &UiGroup::filters);
    }
    c.RegisterArray<Rml::Vector<UiGroup>>();
    c.RegisterArray<Rml::Vector<UiListing>>();
    c.RegisterArray<Rml::Vector<Rml::String>>();
    if (auto h = c.RegisterStruct<UiMapStat>()) {
        h.RegisterMember("label", &UiMapStat::label);
        h.RegisterMember("value", &UiMapStat::value);
    }
    if (auto h = c.RegisterStruct<UiMapMod>()) {
        h.RegisterMember("index", &UiMapMod::index);
        h.RegisterMember("text", &UiMapMod::text);
        h.RegisterMember("mark", &UiMapMod::mark);
        h.RegisterMember("tag", &UiMapMod::tag);
    }
    c.RegisterArray<Rml::Vector<UiMapStat>>();
    c.RegisterArray<Rml::Vector<UiMapMod>>();
    if (auto h = c.RegisterStruct<UiHotkey>()) {
        h.RegisterMember("index", &UiHotkey::index);
        h.RegisterMember("label", &UiHotkey::label);
        h.RegisterMember("desc", &UiHotkey::desc);
        h.RegisterMember("key", &UiHotkey::key);
        h.RegisterMember("old", &UiHotkey::old);
        h.RegisterMember("state", &UiHotkey::state);
        h.RegisterMember("can_clear", &UiHotkey::can_clear);
    }
    c.RegisterArray<Rml::Vector<UiHotkey>>();
    if (auto h = c.RegisterStruct<UiAbout>()) {
        h.RegisterMember("label", &UiAbout::label);
        h.RegisterMember("value", &UiAbout::value);
    }
    c.RegisterArray<Rml::Vector<UiAbout>>();

    c.Bind("name", &model.name);
    c.Bind("base", &model.base);
    c.Bind("subtitle", &model.subtitle);
    c.Bind("rarity", &model.rarity);
    c.Bind("state", &model.state);
    c.Bind("status", &model.status);
    c.Bind("status_error", &model.status_error);
    c.Bind("source", &model.source);
    c.Bind("thin", &model.thin);
    c.Bind("total", &model.total);
    c.Bind("hint", &model.hint);
    c.Bind("closable", &model.closable);
    c.Bind("base_label", &model.base_label);
    c.Bind("category_label", &model.category_label);
    c.Bind("base_preset", &model.base_preset);
    c.Bind("preset_base", &model.preset_base);
    c.Bind("corrupted", &model.corrupted);
    c.Bind("corrupted_label", &model.corrupted_label);
    c.Bind("base_only", &model.base_only);
    c.Bind("searched", &model.searched);
    c.Bind("stale", &model.stale);
    c.Bind("verdict", &model.verdict);
    c.Bind("verdict_kind", &model.verdict_kind);
    c.Bind("map_stats", &model.map_stats);
    c.Bind("map_mods", &model.map_mods);
    c.Bind("profile", &model.profile);
    c.Bind("searching", &model.searching);
    c.Bind("has_url", &model.has_url);
    c.Bind("prices", &model.prices);
    c.Bind("filter_groups", &model.filter_groups);
    c.Bind("listings", &model.listings);
    c.Bind("has_summary", &model.has_summary);
    c.Bind("low_value", &model.low_value);
    c.Bind("summary_price", &model.summary_price);
    c.Bind("summary_icon", &model.summary_icon);
    c.Bind("summary_sub", &model.summary_sub);
    c.Bind("summary_range", &model.summary_range);
    c.Bind("ninja_icon", &model.ninja_icon);
    c.Bind("ninja_meta", &model.ninja_meta);
    c.Bind("debug_lines", &model.debug_lines);
    c.Bind("can_switch", &model.can_switch);
    c.Bind("map_price", &model.map_price);
    c.Bind("map_price_label", &model.map_price_label);
    c.Bind("s_tab", &model.s_tab);
    c.Bind("s_tabs", &model.s_tabs);
    c.Bind("s_profiles", &model.s_profiles);
    c.Bind("s_version", &model.s_version);
    c.Bind("s_hotkeys", &model.s_hotkeys);
    c.Bind("s_warning", &model.s_warning);
    c.Bind("s_warning_title", &model.s_warning_title);
    c.Bind("s_warning_text", &model.s_warning_text);
    c.Bind("s_warning_key", &model.s_warning_key);
    c.Bind("s_league", &model.s_league);
    c.Bind("s_league_pinned", &model.s_league_pinned);
    c.Bind("s_status", &model.s_status);
    c.Bind("s_min", &model.s_min);
    c.Bind("s_scale", &model.s_scale);
    c.Bind("s_scale_env", &model.s_scale_env);
    c.Bind("s_stash_scroll", &model.s_stash_scroll);
    c.Bind("s_profile", &model.s_profile);
    c.Bind("s_reset_armed", &model.s_reset_armed);
    c.Bind("s_about", &model.s_about);
    c.Bind("s_note", &model.s_note);
    c.Bind("s_note_error", &model.s_note_error);
    c.Bind("t_state", &model.t_state);
    c.Bind("t_hint", &model.t_hint);
    c.Bind("t_warn", &model.t_warn);
    c.Bind("t_busy", &model.t_busy);

    // Events arrive while update() feeds pointer input, with price_check set.
    c.BindEventCallback("close", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->close();
    });
    c.BindEventCallback("toggle_preset", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->toggle_preset();
    });
    c.BindEventCallback("toggle_base", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check && price_check->reply()) price_check->set_base_only(!price_check->reply()->base_only);
    });
    c.BindEventCallback("cycle_corrupted", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->cycle_corrupted();
    });
    c.BindEventCallback("search", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->search();
    });
    // Range sliders: a press moves the nearer handle there, dragging moves it on.
    c.BindEventCallback("range_press", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (!args.empty()) range_input(args[0].Get<int>(), ev, true);
    });
    c.BindEventCallback("range_drag", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (!args.empty()) range_input(args[0].Get<int>(), ev, false);
    });
    // bounds_scroll(index, field): the wheel over either half of a filter's
    // value moves that bound one step (also where typing isn't available:
    // see edit_bounds below).
    c.BindEventCallback("bounds_scroll", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (args.size() < 2) return;
        bounds_scroll(args[0].Get<int>(), args[1].Get<int>(), ev.GetParameter<float>("wheel_delta_y", 0));
        ev.StopPropagation();
    });
    // edit_bounds(index, field): clicking either half of a filter's value
    // types into it (where the compositor lets us take the keyboard); the
    // click doesn't also toggle the filter.
    c.BindEventCallback("edit_bounds", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        if (price_check && args.size() >= 2 && ev.GetParameter<int>("button", 0) == 0)
            price_check->edit_bounds(args[0].Get<int>(), args[1].Get<int>());
        ev.StopPropagation();
    });
    c.BindEventCallback("toggle_filter", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList& args) {
        if (price_check && !args.empty()) price_check->toggle_filter(args[0].Get<int>());
    });
    // Bound to mousedown: left button cycles forward, right button back.
    c.BindEventCallback("cycle_mod", [this](Rml::DataModelHandle, Rml::Event& ev, const Rml::VariantList& args) {
        int button = ev.GetParameter<int>("button", 0);
        if (price_check && !args.empty() && button <= 1) price_check->cycle_mod(args[0].Get<int>(), button == 1);
    });
    c.BindEventCallback("set_profile", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList& args) {
        if (price_check && !args.empty()) price_check->set_profile(args[0].Get<int>());
    });
    c.BindEventCallback("switch_view", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->switch_view();
    });
    c.BindEventCallback("open_trade", [this](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList&) {
        if (price_check) price_check->open_trade();
    });

    // Settings page
    auto settings_event = [this, &c](const char* name, auto fn) {
        c.BindEventCallback(name, [this, fn](Rml::DataModelHandle, Rml::Event&, const Rml::VariantList& args) {
            if (price_check) fn(price_check->settings(), args);
        });
    };
    using Args = const Rml::VariantList&;
    auto arg = [](Args args) { return args.empty() ? 0 : args[0].Get<int>(); };
    settings_event("open_settings", [this](SettingsPage&, Args) { price_check->open_settings(); });
    settings_event("settings_close", [](SettingsPage& s, Args) { s.close(); });
    settings_event("settings_tab", [arg](SettingsPage& s, Args a) { s.set_tab((SettingsPage::Tab)arg(a)); });
    settings_event("hotkey_capture", [arg](SettingsPage& s, Args a) {
        auto action = (SettingsPage::Action)arg(a);
        s.capturing() == action ? s.cancel_capture() : s.begin_capture(action);
    });
    settings_event("hotkey_clear", [arg](SettingsPage& s, Args a) { s.clear_hotkey((SettingsPage::Action)arg(a)); });
    settings_event("warning_retry", [](SettingsPage& s, Args) { s.warning_retry(); });
    settings_event("warning_accept", [](SettingsPage& s, Args) { s.warning_accept(); });
    settings_event("warning_keep", [](SettingsPage& s, Args) { s.warning_keep(); });
    settings_event("stash_scroll", [arg](SettingsPage& s, Args a) { s.set_stash_scroll(arg(a) != 0); });
    settings_event("league_step", [arg](SettingsPage& s, Args a) { s.step_league(arg(a)); });
    settings_event("settings_status", [](SettingsPage& s, Args a) {
        if (!a.empty()) s.set_status(a[0].Get<Rml::String>());
    });
    settings_event("min_step", [arg](SettingsPage& s, Args a) { s.step_min(arg(a)); });
    settings_event("scale_step", [arg](SettingsPage& s, Args a) { s.step_scale(arg(a)); });
    settings_event("settings_profile", [arg](SettingsPage& s, Args a) { s.set_map_profile(arg(a)); });
    settings_event("map_reset", [](SettingsPage& s, Args) { s.reset_map_marks(); });
    settings_event("settings_reset", [](SettingsPage& s, Args) { s.reset_defaults(); });
    handle = c.GetModelHandle();
}

void OverlayUi::Impl::load_documents() {
    for (auto& [path, doc] : docs) doc->Close();
    docs.clear();
    Rml::Factory::ClearStyleSheetCache();
    Rml::Factory::ClearTemplateCache();
    for (const char* path : kDocuments) {
        if (auto* doc = context->LoadDocument(path))
            docs[path] = doc;
        else
            poe_log("ui: couldn't load %s", path);
    }
    loaded_mtime = dev_files_mtime();
    revision = UINT64_MAX;  // re-sync visibility
}

time_t OverlayUi::Impl::dev_files_mtime() const {
    if (files.dev_dir.empty()) return 0;
    time_t newest = mtime(files.dev_dir + "/ui/theme.rcss");
    for (const char* path : kDocuments) newest = std::max(newest, mtime(files.dev_dir + "/" + path));
    return newest;
}

void OverlayUi::update(PriceCheck& pc, WaylandInput* input, VkExtent2D extent) {
    Impl& d = *impl_;
    if (!d.ok) return;
    d.frame++;

    // Live editing: reload documents when their files change.
    if (!d.files.dev_dir.empty() && d.frame % 60 == 0 && d.dev_files_mtime() != d.loaded_mtime) {
        poe_log("ui: reloading documents");
        d.load_documents();
    }

    d.context->SetDimensions(Rml::Vector2i((int)extent.width, (int)extent.height));
    if (!d.ui_scale_env) d.ui_scale = (float)pc.settings().values().ui_scale;
    d.context->SetDensityIndependentPixelRatio((input ? (float)input->scale() : 1.0f) * d.ui_scale);

    d.price_check = &pc;
    if (input) {
        for (const PointerEvent& ev : input->take_pointer_events()) {
            switch (ev.type) {
            case PointerEvent::Motion: d.context->ProcessMouseMove((int)ev.x, (int)ev.y, 0); break;
            case PointerEvent::Leave: d.context->ProcessMouseLeave(); break;
            case PointerEvent::Button:
                // Clicking the panel keeps it open (see PriceCheck::lock).
                if (ev.down) pc.lock();
                ev.down ? d.context->ProcessMouseButtonDown(ev.button, 0)
                        : d.context->ProcessMouseButtonUp(ev.button, 0);
                break;
            case PointerEvent::Wheel: d.context->ProcessMouseWheel(Rml::Vector2f(0, -ev.wheel), 0); break;
            }
        }
    }
    d.price_check = nullptr;

    d.sync(pc);
    if (d.debug && d.frame % 30 == 0) d.sync_debug(input, extent);

    for (const char* path : {kPriceDoc, kMapDoc}) {
        auto it = d.docs.find(path);
        if (it == d.docs.end()) continue;
        bool want = pc.visible() && path == d.panel_doc(pc);
        if (want && !it->second->IsVisible()) it->second->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
        if (!want && it->second->IsVisible()) it->second->Hide();
    }
    if (auto it = d.docs.find(kSettingsDoc); it != d.docs.end()) {
        bool want = pc.settings().visible();
        if (want && !it->second->IsVisible()) it->second->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
        if (!want && it->second->IsVisible()) it->second->Hide();
    }
    d.update_toast(pc, extent);
    if (auto it = d.docs.find(kDebugDoc); it != d.docs.end()) {
        if (d.debug && !it->second->IsVisible()) it->second->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
    }

    d.place(pc, extent);
    d.context->Update();
    // Keeping the panel on screen needs its laid-out size; redo the layout
    // if that moved it.
    bool moved = d.keep_on_screen(d.shown_panel, extent);
    if (d.keep_on_screen(kSettingsDoc, extent) || moved) d.context->Update();
    d.mark_cut_mods();
    if (input) input->set_cursor(d.system.cursor);
}

void OverlayUi::render() {
    if (impl_->ok) impl_->context->Render();
}

std::vector<InputRect> OverlayUi::input_rects() const {
    std::vector<InputRect> rects;
    if (!impl_->ok) return rects;
    for (auto& [path, doc] : impl_->docs) {
        // The start-up notice lets clicks through to the game.
        if (!doc->IsVisible() || path == kToastDoc) continue;
        Rml::Vector2f pos = doc->GetAbsoluteOffset(Rml::BoxArea::Border);
        Rml::Vector2f size = doc->GetBox().GetSize(Rml::BoxArea::Border);
        rects.push_back({(int)floorf(pos.x), (int)floorf(pos.y), (int)ceilf(size.x), (int)ceilf(size.y)});
    }
    return rects;
}

void OverlayUi::Impl::sync(PriceCheck& pc) {
    if (pc.revision() == revision && pc.searching() == model.searching) return;
    revision = pc.revision();

    Model m;
    m.debug_lines = std::move(model.debug_lines);
    sync_settings(pc, m);
    const SettingsPage::Values& sv = pc.settings().values();
    // "ready" once prices are in; until then, or without the daemon, only running.
    m.t_busy = pc.daemon_state() != "ready" && !pc.daemon_state().empty();
    if (pc.daemon_state() == "loading") m.t_state = "is running · loading prices";
    else if (pc.daemon_state() == "error") m.t_state = "is running";
    else m.t_state = "is ready";
    m.t_hint = combo_label(sv.hotkeys[(int)SettingsPage::Action::Quick]) + ": price check  ·  " +
               combo_label(sv.hotkeys[(int)SettingsPage::Action::Settings]) + ": settings";
    if (pc.daemon_state() == "error") m.t_warn = pc.daemon_error();
    m.searching = pc.searching();
    bool map = pc.purpose() == PriceCheck::Purpose::Map;
    // Only a locked panel (Ctrl+Alt+D, Alt+D) needs a close button: the
    // others close by themselves when the mouse moves away.
    m.closable = pc.mode() == PriceCheck::Mode::Locked;
    switch (pc.mode()) {
    case PriceCheck::Mode::Quick: {
        // The quick look key's modifiers (Ctrl) keep it open.
        Rml::String hold = mods_label(pc.hold_mods());
        Rml::String start = hold.empty() ? "Move" : "Hold " + hold + " and move";
        m.hint = start + (map ? " here to mark mods" : " here to use filters");
        break;
    }
    // Same line count as in Quick, so pinning doesn't resize the panel
    // (and pull it out from under the pointer).
    case PriceCheck::Mode::Pinned: {
        Rml::String hold = mods_label(pc.hold_mods());
        m.hint = hold.empty() ? "Move away to close" : "Release " + hold + " and move away to close";
        break;
    }
    case PriceCheck::Mode::Locked:
        if (map) m.hint = "Click a mod to mark it · right-click to go back";
        else if (const auto& e = pc.bounds_edit(); e && !pc.keyboard_taken())
            m.hint = "Taking the keyboard from the game…";
        else if (e)
            m.hint = "Type a number · Tab: min/max · Enter: apply · Esc: cancel";
        break;
    }
    const auto& item = pc.item();
    const auto& reply = pc.reply();
    m.can_switch = pc.can_switch_view();

    if (item) {
        m.name = item->name;
        m.base = item->base;
        m.rarity = rarity_class(item->rarity);
        // "Rare · Rings", but just "Stackable Currency" / "Skill Gems".
        bool redundant = item->rarity.empty() || item->item_class.find(item->rarity) != std::string::npos;
        m.subtitle = redundant ? item->item_class : item->rarity + " · " + item->item_class;
        if (!item->item_level.empty()) m.subtitle += " · ilvl " + item->item_level;
    }

    if (!item || !reply) {
        m.state = "status";
        m.status = !pc.status().empty() ? pc.status()
                   : m.searching         ? (map ? "Checking map…" : "Checking price…")
                                         : (map ? "No map check" : "No price");
        m.status_error = !pc.status().empty();
    } else if (reply->status == "map") {
        m.state = "map";
        if (!reply->subtitle.empty()) m.subtitle = reply->subtitle;
        sync_map(*reply, pc.check_id(), m);
        if (!reply->prices.empty()) {
            const PriceLine& p = reply->prices[0];
            m.map_price = p.divine >= 1.0 ? format(p.divine >= 10 ? "%.0f div" : "%.1f div", p.divine) : chaos_text(p.chaos);
            m.map_price_label = p.label;
        }
    } else if (reply->status == "not_map") {
        m.state = "status";
        m.status = "Not a map. The map check is for maps, invitations, contracts and logbooks.";
    } else if (reply->status == "trade") {
        m.state = "trade";
        m.status = pc.status();
        m.total = reply->total;
        m.has_url = !reply->url.empty();
        m.searched = reply->searched;
        m.stale = pc.stale();
        m.base_only = reply->base_only;
        m.base_label = reply->base_type;
        m.base_preset = reply->base_preset;
        m.preset_base = reply->preset == "base";
        // The base item preset always searches the exact base.
        if (m.preset_base) m.base_only = true;
        if (!reply->category_label.empty()) m.category_label = "Any " + reply->category_label;
        // Short, so the button keeps its size; its colour tells the state too.
        m.corrupted = reply->corrupted;
        m.corrupted_label = reply->corrupted == "no" ? "Uncorrupted" : reply->corrupted == "yes" ? "Corrupted" : "Any";
        m.source = reply->league;
        // A unique's poe.ninja price, which doesn't know its rolls.
        fill_prices(*reply, m);
        if (!reply->prices.empty()) {
            const PriceLine& p = reply->prices[0];
            m.ninja_icon = m.prices[0].icon;
            const Rml::String& listed = p.listings;
            if (!listed.empty() && atoi(listed.c_str()) < 5) m.thin = listed + " listed";
            m.ninja_meta = (listed.empty() ? "" : listed + " listed \u00b7 ") + "poe.ninja";
            if (item && item->rarity == "Unique") m.ninja_meta += " \u00b7 rolls not counted";
        }
        // Groups in panel order; the top one takes its name from what's in it.
        auto has = [&](const char* section) {
            return std::any_of(reply->filters.begin(), reply->filters.end(),
                               [&](const TradeFilter& f) { return f.section == section; });
        };
        Rml::String top = m.preset_base ? "Base item" : has("weapon") ? "Weapon" : has("armour") ? "Armour" : has("map") ? "Map"
                        : has("gem")       ? "Gem"
                                           : "Item";
        m.filter_groups = {{top, {}}, {"Enchant", {}}, {"Implicit", {}}, {"Totals", {}}, {"Mods", {}}};
        for (const TradeFilter& f : reply->filters) {
            UiFilter u{f.index, f.enabled, f.label, f.min};
            u.has_slider = f.hi > f.lo && !f.min.empty();
            if (u.has_slider) {
                auto pct = [&](double v) { return (float)std::clamp((v - f.lo) / (f.hi - f.lo) * 100, 0.0, 100.0); };
                u.min_pct = pct(atof(f.min.c_str()));
                u.max_pct = f.max.empty() ? 100 : pct(atof(f.max.c_str()));
                if (!f.roll.empty()) u.roll_pct = pct(atof(f.roll.c_str()));
            }
            u.split = !f.min.empty();
            if (u.split) {
                u.lo = f.min;
                u.hi = f.max;
            } else if (!f.max.empty()) {
                u.bounds = "\u2264 " + f.max;
            }
            // Being typed into: what's typed so far, a caret in its half.
            if (const auto& e = pc.bounds_edit(); e && e->index == f.index) {
                u.edit_field = e->field;
                u.lo = e->min + (e->field == 0 ? "|" : "");
                u.hi = e->max + (e->field == 1 ? "|" : "");
            }
            u.hi_empty = u.hi.empty();
            if (u.hi_empty) u.hi = "max";
            u.max_roll = f.max_roll;
            if (f.tier > 0) u.tier = "T" + std::to_string(f.tier);
            u.section = f.section;
            const std::string& sec = f.section;
            size_t group = sec == "enchant"                                         ? 1
                         : sec == "implicit" || sec == "exarch" || sec == "eater" ? 2
                         : sec == "total"                                         ? 3
                         : sec == "mod" || sec == "fractured" || sec == "crafted" ? 4 : 0;
            m.filter_groups[group].filters.push_back(u);
        }
        // Each eldritch implicit's tier row next to its mods: plain, Exarch, Eater.
        auto rank = [](const UiFilter& f) { return f.section == "exarch" ? 1 : f.section == "eater" ? 2 : 0; };
        std::stable_sort(m.filter_groups[2].filters.begin(), m.filter_groups[2].filters.end(),
                         [&](const UiFilter& a, const UiFilter& b) { return rank(a) < rank(b); });
        m.filter_groups.erase(std::remove_if(m.filter_groups.begin(), m.filter_groups.end(),
                                             [](const UiGroup& g) { return g.filters.empty(); }),
                              m.filter_groups.end());
        for (const TradeListing& l : reply->listings) {
            UiListing u;
            u.price = l.amount + " " + l.currency;
            // Skip conversions too small to read ("0.0c").
            if (l.chaos >= 0.1 && l.currency != "Chaos Orb") u.sub = chaos_text(l.chaos);
            u.age = l.age;
            if (l.count > 1) u.count = "×" + std::to_string(l.count);
            if (icons_get(l.currency_id)) u.icon = l.currency_id;
            // A gem's level and quality, on its row (like APT's columns).
            for (const auto& p : l.preview)
                if (p.kind == "gem") u.tag = p.text;
            fill_preview(l, u);
            m.listings.push_back(u);
        }
        if (reply->summary) {
            const TradeSummary& s = *reply->summary;
            m.has_summary = true;
            m.summary_price = (s.capped ? "≤ " : "≈ ") + s.amount + " " + s.currency;
            if (icons_get(s.currency_id)) m.summary_icon = s.currency_id;
            if (s.chaos >= 0.1 && s.currency != "Chaos Orb") m.summary_sub = chaos_text(s.chaos);
            m.low_value = s.chaos >= 0 && s.chaos < 5;
            m.summary_range = std::to_string(count_listings(*reply)) + " of " + thousands(reply->total) + " · " + s.range;
        }
    } else if (reply->status == "ok") {
        m.state = "prices";
        fill_prices(*reply, m);
        Rml::String listed = reply->prices.empty() ? "" : reply->prices[0].listings;
        bool thin = !listed.empty() && atoi(listed.c_str()) < 5;
        if (thin) m.thin = listed + " listed · thin";
        m.source = (!listed.empty() && !thin ? listed + " listed · " : "") + "poe.ninja · " + reply->league;
        if (reply->age_minutes >= 0) m.source += " · " + std::to_string(reply->age_minutes) + " min ago";
    } else {
        m.state = "status";
        m.status = !reply->message.empty() ? reply->message : reply->status;
        m.status_error = reply->status == "error";
    }

    model = std::move(m);
    handle.DirtyAllVariables();
}

void OverlayUi::Impl::sync_settings(PriceCheck& pc, Model& m) {
    const SettingsPage& s = pc.settings();
    const SettingsPage::Values& v = s.values();
    using Action = SettingsPage::Action;
    m.s_tab = (int)s.tab();
    m.s_tabs = {"Hotkeys", "Price check", "Map check", "About"};
    m.s_profiles = {"I", "II", "III"};
    m.s_version = POE_WAYTRADE_VERSION;

    static const char* const descs[SettingsPage::kActions] = {
        "Price of the hovered item; closes when the mouse moves away",
        "Stays open until × or a click in the game",
        "Same as the locked panel, for old habits",
        "This page",
    };
    for (int i = 0; i < SettingsPage::kActions; i++) {
        UiHotkey h;
        h.index = i;
        h.label = SettingsPage::action_label((Action)i);
        h.desc = descs[i];
        h.old = v.hotkeys[i].empty() ? "it off" : combo_label(v.hotkeys[i]);
        h.key = v.hotkeys[i].empty() ? "Off" : combo_label(v.hotkeys[i]);
        h.can_clear = i == (int)Action::MapAlias && !v.hotkeys[i].empty();
        if (v.hotkeys[i].empty()) h.state = "off";
        if (s.capturing() == (Action)i) {
            h.key = "Press keys…";
            h.state = "capturing";
            h.can_clear = false;
        }
        if (s.warning() && (int)s.warning()->action == i) {
            h.key = combo_label(s.warning()->combo);
            h.state = "warned";
            h.can_clear = false;
        }
        m.s_hotkeys.push_back(h);
    }
    if (const auto& w = s.warning()) {
        m.s_warning = (int)w->action;
        m.s_warning_key = combo_label(w->combo);
        m.s_warning_title = m.s_warning_key + " also reaches the game";
        m.s_warning_text = "The game keeps the keyboard, so every press would do both. Add Ctrl or Alt to keep it to "
                           "the overlay.";
    }

    if (v.league == "auto")
        m.s_league = s.league_active().empty() ? "Current" : "Current (" + s.league_active() + ")";
    else
        m.s_league = v.league;
    m.s_league_pinned = s.league_pinned();
    m.s_status = v.status;
    m.s_min = std::to_string(100 - v.min_percent) + "%";
    m.s_scale = std::to_string((int)std::lround(v.ui_scale * 100)) + "%";
    m.s_scale_env = ui_scale_env;
    m.s_stash_scroll = v.stash_scroll;
    m.s_profile = s.map_profile();
    m.s_reset_armed = s.reset_armed();

    m.s_about.push_back({"Overlay", POE_WAYTRADE_VERSION});
    m.s_about.push_back({"Daemon", s.loaded() ? s.daemon_version() : "not answering"});
    m.s_about.push_back({"Prices for", s.league_active().empty() ? "(loading)" : s.league_active()});
    if (!s.daemon_log().empty()) m.s_about.push_back({"Daemon log", s.daemon_log()});
    const char* log = getenv("POE_WAYTRADE_LOG");
    m.s_about.push_back({"Overlay log", log && *log ? log : "off (set POE_WAYTRADE_LOG)"});
    // Only looked up on this tab: it connects to the compositor once.
    if (s.tab() == SettingsPage::Tab::About)
        m.s_about.push_back({"Typing in panels", pc.typing_supported()
                                                     ? "yes (layer-shell)"
                                                     : "no: needs KDE Plasma, Sway, Hyprland or another "
                                                       "compositor with layer-shell"});
    m.s_note = s.note();
    m.s_note_error = s.note_error();
}

void OverlayUi::Impl::update_toast(PriceCheck& pc, VkExtent2D extent) {
    auto it = docs.find(kToastDoc);
    if (it == docs.end()) return;
    Rml::ElementDocument* doc = it->second;
    double now = system.GetElapsedTime();
    if (toast_started < 0 && !toast_off &&
        (pc.settings().loaded() || !pc.daemon_state().empty() || now > kToastWait)) {
        toast_started = now;
        poe_log("ui: start-up notice");
    }
    double t = toast_started < 0 ? -1 : now - toast_started;
    if (t < 0 || t > kToastShow) {
        if (doc->IsVisible()) doc->Hide();
        return;
    }
    if (!doc->IsVisible()) doc->Show(Rml::ModalFlag::None, Rml::FocusFlag::None);
    // Rises into place and fades in, then sinks and fades out.
    auto ease_out = [](double x) { return 1 - std::pow(1 - std::clamp(x, 0.0, 1.0), 3); };
    double in = ease_out(t / kToastIn), out = ease_out((t - (kToastShow - kToastFade)) / kToastFade);
    float dp = context->GetDensityIndependentPixelRatio();
    float alpha = (float)(in * (1 - out));
    float rise = (float)((1 - in) * 18 + out * 10) * dp;
    float scale = (float)(0.96 + 0.04 * in);
    doc->SetProperty(Rml::PropertyId::Opacity, Rml::Property(alpha, Rml::Unit::NUMBER));
    char transform[64];
    snprintf(transform, sizeof transform, "translateY(%.1fpx) scale(%.3f)", rise, scale);
    doc->SetProperty("transform", transform);
    // Centred just under PoE's login box (it ends at 87% of the height, on
    // any window size), so it's seen at the login screen without covering it.
    Rml::Vector2f size = doc->GetBox().GetSize(Rml::BoxArea::Border);
    float top = std::min(extent.height * 0.885f, extent.height - size.y - 8 * dp);
    doc->SetProperty(Rml::PropertyId::Left, Rml::Property(std::max(0.0f, (extent.width - size.x) / 2), Rml::Unit::PX));
    doc->SetProperty(Rml::PropertyId::Top, Rml::Property(top, Rml::Unit::PX));
}

// Like Awakened PoE Trade: an item checked in the inventory (right half)
// gets the panel right against the inventory's left edge, one in the stash
// (left half) right against the stash's right edge. PoE's side panels are
// 986/1600 of the window height wide.
void OverlayUi::Impl::place(PriceCheck& pc, VkExtent2D extent) {
    shown_panel = panel_doc(pc);
    if (pc.check_id() == placed_check) return;
    placed_check = pc.check_id();
    auto it = docs.find(shown_panel);
    float ax, ay;
    if (it == docs.end() || !pc.anchor(&ax, &ay)) return;
    float panel = extent.height * 986.0f / 1600.0f;
    float width = 370 * context->GetDensityIndependentPixelRatio();
    bool by_inventory = ax > extent.width / 2.0f;
    float left = by_inventory ? extent.width - panel - width : panel;
    // Listing previews open away from the inventory/stash the panel sits on.
    it->second->SetClass("preview-right", !by_inventory);
    it->second->SetProperty(Rml::PropertyId::Left, Rml::Property(left, Rml::Unit::PX));
    it->second->SetProperty(Rml::PropertyId::Top, Rml::Property(extent.height * 0.08f, Rml::Unit::PX));
}

// Price check mods that don't fit on their line (they end in "…") get
// "cut", which lets hovering show their full text. Takes effect next frame.
void OverlayUi::Impl::mark_cut_mods() {
    auto it = docs.find(kPriceDoc);
    if (it == docs.end() || !it->second->IsVisible()) return;
    Rml::ElementList mods;
    it->second->GetElementsByClassName(mods, "mod");
    for (Rml::Element* mod : mods) {
        bool cut = mod->GetScrollWidth() > mod->GetClientWidth() + 0.5f;
        if (Rml::Element* cell = mod->GetParentNode(); cell && cell->IsClassSet("cut") != cut) cell->SetClass("cut", cut);
    }
}

bool OverlayUi::Impl::keep_on_screen(const char* path, VkExtent2D extent) {
    auto it = docs.find(path);
    if (it == docs.end() || !it->second->IsVisible()) return false;
    Rml::ElementDocument* doc = it->second;
    float margin = 16 * context->GetDensityIndependentPixelRatio();
    Rml::Vector2f pos = doc->GetAbsoluteOffset(Rml::BoxArea::Border);
    Rml::Vector2f size = doc->GetBox().GetSize(Rml::BoxArea::Border);
    float x = std::clamp(pos.x, margin, std::max(margin, extent.width - margin - size.x));
    float y = std::clamp(pos.y, margin, std::max(margin, extent.height - margin - size.y));
    if (std::abs(x - pos.x) < 0.5f && std::abs(y - pos.y) < 0.5f) return false;
    doc->SetProperty(Rml::PropertyId::Left, Rml::Property(x, Rml::Unit::PX));
    doc->SetProperty(Rml::PropertyId::Top, Rml::Property(y, Rml::Unit::PX));
    return true;
}

// Marks for the active profile, dangerous mods first, and a one-line verdict
// readable at a glance. Both stay one line/one order while the panel is open,
// so marking a mod doesn't move anything.
// Like a PoE tooltip: name and base, item level (and DPS), then the mods
// with a divider after the enchants/implicits.
void OverlayUi::Impl::fill_preview(const TradeListing& l, UiListing& u) {
    Rml::String ilvl, dps, sockets;
    Rml::Vector<UiPreviewLine> influences;
    bool before_explicits = false;
    for (const auto& [kind, text, tier] : l.preview) {
        if (kind == "rarity") u.rarity = text;
        else if (kind == "name") u.name = text;
        else if (kind == "base") u.base = text;
        else if (kind == "ilvl" && u.rarity != "gem") ilvl = "Item level " + text;
        else if (kind == "gem") ilvl = text;  // "Level 20 · 23%"
        else if (kind == "dps") dps = text;
        else if (kind == "sockets") sockets = text;
        else if (kind == "icon") {
            if (Rml::String id = item_icon_id(text); icons_get(id)) u.item_icon = id;
        } else if (kind == "cells") {
            // PoE's inventory cells, a little smaller.
            int w = 1, h = 1;
            sscanf(text.c_str(), "%d %d", &w, &h);
            u.icon_w = kPreviewCell * std::clamp(w, 1, 4);
            u.icon_h = kPreviewCell * std::clamp(h, 1, 6);
        } else if (kind == "influence") {
            Rml::String name = text;
            if (!name.empty()) name[0] = (char)toupper((unsigned char)name[0]);
            influences.push_back({"influence", name + " Item", ""});
        } else {
            bool top = kind == "enchant" || kind == "implicit";
            if (top) before_explicits = true;
            else if (before_explicits) {
                u.lines.push_back({"divider", "", ""});
                before_explicits = false;
            }
            u.lines.push_back({kind, text, tier > 0 ? "T" + std::to_string(tier) : ""});
        }
    }
    // Like PoE's item text: influences after the mods.
    if (!influences.empty()) {
        auto end = std::find_if(u.lines.begin(), u.lines.end(), [](const UiPreviewLine& p) {
            return p.kind == "mirrored" || p.kind == "corrupted" || p.kind == "unidentified";
        });
        u.lines.insert(end, influences.begin(), influences.end());
    }
    // Sockets go on the image, like in game; in words until it and the
    // socket images are here.
    bool drawn = !u.item_icon.empty() && u.icon_w > 0 && icons_get(kSocketSprite);
    if (drawn) place_sockets(sockets, u);
    for (const Rml::String& part : {ilvl, drawn ? "" : sockets, dps})
        if (!part.empty()) u.meta += (u.meta.empty() ? "" : " · ") + part;
    u.has_preview = !u.base.empty();
    if (l.count > 1)
        u.more = "+ " + std::to_string(l.count - 1) + " more at this price";
}

// As the trade site lays sockets out (its CSS, at 47.46px a cell): a
// one-cell-wide item stacks them; a wider one fills two columns, snaking
// (left-right, then right-left), the block centred on the item. Images from
// the socket sprite sheet (daemon/src/trade.rs SOCKET_SPRITE).
void OverlayUi::Impl::place_sockets(const Rml::String& text, UiListing& u) {
    // "R-G-B B" -> colours, and whether each links to the one before.
    Rml::Vector<std::pair<Rml::String, bool>> sockets;
    bool linked = false;
    Rml::String colour;
    for (size_t i = 0; i <= text.size(); i++) {
        char c = i < text.size() ? text[i] : ' ';
        if (c != '-' && c != ' ') {
            colour += c;
            continue;
        }
        if (!colour.empty()) sockets.push_back({colour, linked});
        colour.clear();
        linked = c == '-';
    }
    if (sockets.empty()) return;
    // Sprite sheet parts: sockets by colour (abyssal, resonator too), links.
    auto socket_rect = [](const Rml::String& c) {
        return c == "R" ? "140 35 35 35" : c == "G" ? "70 105 35 35" : c == "B" ? "105 35 35 35"
             : c == "A" ? "35 70 35 35" : c == "DV" ? "105 70 35 35" : "140 70 35 35";
    };
    const float scale = kPreviewCell / 47.4645f;
    int n = (int)sockets.size();
    int cols = u.icon_w > kPreviewCell * 1.5f ? std::min(2, n) : 1;
    int rows = (n + cols - 1) / cols;
    float ox = (u.icon_w - cols * kPreviewCell) / 2, oy = (u.icon_h - rows * kPreviewCell) / 2;
    Rml::Vector<Rml::Vector2f> centres;
    for (int i = 0; i < n; i++) {
        int row = i / cols, col = i % cols;
        if (row % 2 == 1 && cols == 2) col = 1 - col;
        Rml::Vector2f c{ox + (col + 0.5f) * kPreviewCell, oy + (row + 0.5f) * kPreviewCell};
        centres.push_back(c);
        float size = 35 * scale;
        u.sockets.push_back({c.x - size / 2, c.y - size / 2, size, size, socket_rect(sockets[i].first)});
    }
    for (int i = 1; i < n; i++) {
        if (!sockets[i].second) continue;
        Rml::Vector2f mid = (centres[i - 1] + centres[i]) * 0.5f;
        bool across = std::abs(centres[i].x - centres[i - 1].x) > 1;
        float w = (across ? 38 : 15) * scale, h = (across ? 15 : 38) * scale;
        u.sockets.push_back({mid.x - w / 2, mid.y - h / 2, w, h, across ? "175 0 38 15" : "175 15 15 38"});
    }
}

void OverlayUi::Impl::fill_prices(const PriceReply& reply, Model& m) {
    for (const PriceLine& p : reply.prices) {
        UiPrice u;
        u.label = p.label;
        if (p.divine >= 1.0) {
            u.main = format(p.divine >= 10 ? "%.0f div" : "%.1f div", p.divine);
            u.sub = chaos_text(p.chaos);
        } else {
            u.main = chaos_text(p.chaos);
        }
        if (const char* id = price_icon(p); icons_get(id)) u.icon = id;
        m.prices.push_back(u);
    }
}

void OverlayUi::Impl::sync_map(const PriceReply& reply, uint64_t check_id, Model& m) {
    m.profile = std::clamp(reply.profile, 1, 3);
    for (const MapStat& s : reply.map_stats) {
        Rml::String label = s.label;
        if (label == "Item Quantity") label = "Quantity";
        else if (label == "Item Rarity") label = "Rarity";
        else if (label == "Monster Pack Size") label = "Pack size";
        m.map_stats.push_back({label, s.value});
    }

    int deadly = 0, warnings = 0, good = 0, unmarked = 0;
    std::vector<UiMapMod> mods;
    for (size_t i = 0; i < reply.map_mods.size(); i++) {
        const MapMod& mod = reply.map_mods[i];
        char mark = (size_t)(m.profile - 1) < mod.marks.size() ? mod.marks[m.profile - 1] : '-';
        UiMapMod u{(int)i, mod.text, "", ""};
        if (mark == 'd') u.mark = "deadly", u.tag = "Deadly", deadly++;
        else if (mark == 'w') u.mark = "warning", u.tag = "Warning", warnings++;
        else if (mark == 'g') u.mark = "good", u.tag = "Good", good++;
        else unmarked++;
        mods.push_back(u);
    }

    if (check_id != map_order_check || m.profile != map_order_profile || map_order.size() != mods.size()) {
        map_order_check = check_id;
        map_order_profile = m.profile;
        auto rank = [](const UiMapMod& u) { return u.mark == "deadly" ? 0 : u.mark == "warning" ? 1 : u.mark == "good" ? 2 : 3; };
        std::vector<UiMapMod> sorted = mods;
        std::stable_sort(sorted.begin(), sorted.end(),
                         [&](const UiMapMod& a, const UiMapMod& b) { return rank(a) < rank(b); });
        map_order.clear();
        for (const UiMapMod& u : sorted) map_order.push_back(u.index);
    }
    for (int i : map_order) m.map_mods.push_back(mods[i]);

    auto count = [](int n, const char* one, const char* many) { return std::to_string(n) + " " + (n == 1 ? one : many); };
    if (deadly || warnings) {
        m.verdict_kind = deadly ? "deadly" : "warning";
        if (deadly) m.verdict = count(deadly, "deadly mod", "deadly mods");
        if (warnings) m.verdict += (deadly ? " · " : "") + count(warnings, "warning", "warnings");
    } else {
        m.verdict_kind = "clean";
        m.verdict = reply.map_mods.empty() ? "No mods" : "No dangerous mods";
    }
    if (good) m.verdict += " · " + count(good, "good mod", "good mods");
    // Mods nobody has judged yet are easy to skim past: count them too.
    if (unmarked) m.verdict += " · " + count(unmarked, "unmarked", "unmarked");
}

void OverlayUi::Impl::sync_debug(WaylandInput* input, VkExtent2D extent) {
    Rml::Vector<Rml::String> lines;
    char buf[160];
    snprintf(buf, sizeof(buf), "Swapchain %ux%u", extent.width, extent.height);
    lines.push_back(buf);
    if (input) {
        snprintf(buf, sizeof(buf), "Wayland: %s, focus %s", input->status(), input->focused() ? "yes" : "no");
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Last key %u %s", input->last_key(), input->last_key_down() ? "down" : "up");
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Clipboard #%llu, %zu bytes", (unsigned long long)input->clipboard_seq(),
                 input->clipboard_text().size());
        lines.push_back(buf);
        snprintf(buf, sizeof(buf), "Pointer over overlay: %s, scale %.3f", input->pointer_inside() ? "yes" : "no",
                 input->scale());
        lines.push_back(buf);
    } else {
        lines.push_back("No Wayland input (X11 isn't supported)");
    }
    if (lines != model.debug_lines) {
        model.debug_lines = std::move(lines);
        handle.DirtyVariable("debug_lines");
    }
}

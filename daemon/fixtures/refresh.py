#!/usr/bin/env python3
"""Re-captures the API replies the daemon's tests run against, trimmed to
what the tests use. Run it from anywhere; it writes next to itself:

    python3 daemon/fixtures/refresh.py          # PoE1's and PoE2's
    python3 daemon/fixtures/refresh.py --poe2   # PoE2's only (fixtures/poe2/)

then `cargo test`. A test that fails afterwards means poe.ninja or the trade
site changed something the daemon relies on (or the market moved: prices in
assertions are read from the fixtures, not hard-coded, where possible).

Makes about a dozen requests: a few to poe.ninja, two data requests, one
search and one fetch on pathofexile.com. Listings keep only their price and
time; sellers' accounts are dropped.
"""

import json
import pathlib
import time
import urllib.parse
import urllib.request

OUT = pathlib.Path(__file__).resolve().parent
UA = "poe-waytrade-daemon/tests (personal price-check overlay)"
NINJA = "https://poe.ninja/poe1/api"
TRADE = "https://www.pathofexile.com/api/trade"
NINJA2 = "https://poe.ninja/poe2/api"
TRADE2 = "https://www.pathofexile.com/api/trade2"
# PoE2's exchange currencies the tests use.
CURRENCIES2 = {"exalted", "divine", "chaos", "alch", "regal", "annul", "vaal", "mirror", "transmute", "aug"}
# PoE2 stat texts the tests' items use (fixtures/poe2/items), in every
# group. PoE2's have no "+".
STAT_TEXTS2 = {
    "#% increased Physical Damage",
    "# to Armour",
    "# to Armour (Local)",
    "#% increased Armour",
    "#% increased Armour (Local)",
    "#% increased Armour, Evasion and Energy Shield",
    "# to Accuracy Rating",
    "# to Accuracy Rating (Local)",
    "#% to Critical Hit Chance",
    "# to Spirit",
    "# to Level of all Spell Skills",
    "+# total maximum Life",
    "+#% total Elemental Resistance",
}

# The exchange currencies the tests price things in.
CURRENCIES = {"chaos", "divine", "alch", "exalted", "mirror", "alt", "chance", "fusing", "jew", "chrome", "vaal",
              "regal", "scour", "gcp", "chisel", "regret", "annul", "blessed", "transmute", "aug"}

# Stat texts the tests' items use (trade.rs RING), in every group, and the
# pseudo totals trade.rs looks up by name.
STAT_TEXTS = {
    "+# to maximum Life",
    "+# to maximum Mana",
    "+#% to Cold Resistance",
    "+#% to Fire Resistance",
    "+# to Strength",
    "+# to Intelligence",
    "+# to Dexterity and Intelligence",
    "#% increased Rarity of Items found",
    "+# total maximum Life",
    "+#% total Elemental Resistance",
    "+#% total to Chaos Resistance",
    "+# total maximum Mana",
    "+# total to Strength",
    "+# total to Dexterity",
    "+# total to Intelligence",
    # Cluster jewels, anointments and eldritch implicits (option stats and
    # pseudo tiers).
    "Adds # Passive Skills",
    "# Added Passive Skills are Jewel Sockets",
    "Added Small Passive Skills grant: 10% increased Spell Damage",
    "Added Small Passive Skills grant: 12% increased Damage with Bows\n"
    "Added Small Passive Skills grant: 12% increased Damage Over Time with Bow Skills",
    "Allocates Charisma",
    "Searing Exarch Implicit Modifier (#)",
    "Eater of Worlds Implicit Modifier (#)",
    # Texts with more than one stat: a tincture's and a flask's, and alike
    # stats searched as either.
    "#% increased effect",
    "#% increased Cooldown Recovery Rate",
    "Regenerate # Energy Shield per second",
    "Melee Weapon Damage Penetrates #% Elemental Resistances",
    # Stats with fixed numbers in their text.
    "#% chance to gain Onslaught for 4 seconds on Kill",
    "Gain Unholy Might for 4 seconds on Critical Strike",
}

# Fields the daemon doesn't read and that only make the files big.
DROP = {"icon", "image", "sparkLine", "sparkline", "lowConfidenceSparkLine", "flavourText", "tradeInfo", "tradeFilter"}
# Mod lists: kept (as their texts) only for uniques, whose variants they tell apart.
MODS = ("implicitModifiers", "explicitModifiers", "mutatedModifiers")


def get(url, body=None):
    time.sleep(0.5)  # be polite
    req = urllib.request.Request(url, data=body and json.dumps(body).encode(), headers={"User-Agent": UA})
    if body:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)


def save(name, value):
    (OUT / name).parent.mkdir(exist_ok=True)
    (OUT / name).write_text(json.dumps(value, indent=1, ensure_ascii=False) + "\n")
    print(f"wrote {name}")


def slim(d, mods=False):
    out = {k: v for k, v in d.items() if k not in DROP and k not in MODS}
    if mods:
        out |= {k: [{"text": m["text"]} for m in d[k]] for k in MODS if d.get(k)}
    return out


def main():
    index = get(f"{NINJA}/data/index-state")
    leagues = [lg["name"] for lg in index["economyLeagues"]]
    save("ninja-index-state.json", {"economyLeagues": [{"name": n} for n in leagues]})
    league = next(n for n in leagues if "Hardcore" not in n and "Ruthless" not in n and n != "Standard")
    q = urllib.parse.quote(league)

    ex = get(f"{NINJA}/economy/exchange/current/overview?league={q}&type=Currency")
    save("ninja-exchange-currency.json", {
        "core": slim(ex["core"]) | {"items": [slim(i) for i in ex["core"].get("items", [])]},
        "lines": [slim(line) for line in ex["lines"] if line["id"] in CURRENCIES],
        "items": [slim(i) for i in ex["items"] if i["id"] in CURRENCIES],
    })

    for kind, name, keep in [
        ("UniqueArmour", "ninja-unique-armour.json",
         lambda line: line["name"] in ("Queen of the Forest", "Foulborn Queen of the Forest", "Ralakesh's Impatience")
         or line.get("baseType") == "Spidersilk Robe"),
        # Pillars of Arun's lines name different tiers but share their art.
        ("UniqueMap", "ninja-unique-map.json",
         lambda line: "Pillars of Arun" in line["name"] or line["name"] == "Cortex"),
        ("SkillGem", "ninja-skill-gem.json", lambda line: line["name"] == "Arc"),
        ("Map", "ninja-map.json", lambda line: line["name"] == "Nightmare Map"),
        ("Viscera", "ninja-viscera.json", lambda line: line["name"] == "Coven Queen's Crown"),
        ("ClusterJewel", "ninja-cluster-jewel.json",
         lambda line: line["name"] in ("10% increased Spell Damage", "Minions deal 10% increased Damage")),
        ("BaseType", "ninja-base-type.json",
         lambda line: line["name"] in ("Vaal Regalia", "Simplex Amulet")),
        ("Wombgift", "ninja-wombgift.json", lambda line: line["name"] == "Ancient Wombgift"),
    ]:
        ov = get(f"{NINJA}/economy/stash/current/item/overview?league={q}&type={kind}")
        lines = [slim(line, mods=kind.startswith("Unique")) for line in ov["lines"] if keep(line)]
        # Viscera have a line per implicit: the most listed are enough.
        lines.sort(key=lambda line: -(line.get("listingCount") or 0))
        save(name, {"lines": lines[:12]} if kind == "Viscera" else {"lines": lines})

    # Valdo maps: a line per map listed. Those of the reward with the most.
    ov = get(f"{NINJA}/economy/stash/current/item/overview?league={q}&type=ValdoMap")
    rewards = [line["variant"] for line in ov["lines"]]
    reward = max(set(rewards), key=rewards.count)
    save("ninja-valdo-map.json", {"lines": [slim(line) for line in ov["lines"] if line["variant"] == reward]})

    stats = get(f"{TRADE}/data/stats")
    save("trade-stats.json", {"result": [
        {"id": g["id"], "label": g["label"], "entries": [e for e in g["entries"] if e["text"] in STAT_TEXTS]}
        for g in stats["result"]
    ]})
    items = get(f"{TRADE}/data/items")
    save("trade-items.json", {"result": [
        {"id": g["id"], "label": g["label"], "entries": [e for e in g["entries"] if e.get("type") == "Arc"]}
        for g in items["result"] if g["id"] == "gem"
    ]})
    static = get(f"{TRADE}/data/static")
    save("trade-static.json", {"result": [
        {"id": g["id"], "label": g.get("label"), "entries": [e for e in g["entries"] if e["id"] in CURRENCIES]}
        for g in static["result"] if g["id"] == "Currency"
    ]})

    # A plain, busy search: cheap rare Sapphire Rings with life.
    search = get(f"{TRADE}/search/{q}", {
        "query": {"status": {"option": "securable"}, "type": "Sapphire Ring",
                  "filters": {"type_filters": {"filters": {"rarity": {"option": "rare"}}}},
                  "stats": [{"type": "and", "filters": [{"id": "pseudo.pseudo_total_life", "value": {"min": 60}}]}]},
        "sort": {"price": "asc"},
    })
    hashes = search["result"][:10]
    save("trade-search.json", {"id": search["id"], "complexity": search.get("complexity"), "result": hashes,
                               "total": search["total"], "inexact": search.get("inexact", False)})
    # Named in the fetch, the search's pseudo total comes with each item
    # (pseudoMods), as the daemon asks for it.
    fetched = get(f"{TRADE}/fetch/{','.join(hashes)}?query={search['id']}&pseudos[]=pseudo.pseudo_total_life")
    entries = []
    for e in fetched["result"]:
        item = {k: v for k, v in e["item"].items() if k != "icon"}
        item.get("extended", {}).pop("text", None)
        listing = {"indexed": e["listing"]["indexed"], "price": e["listing"]["price"]}
        entries.append({"id": e["id"], "listing": listing, "item": item})
    save("trade-fetch.json", {"result": entries})


def main_poe2():
    index = get(f"{NINJA2}/data/index-state")
    # "indexed" isn't whether a league has prices: the current league can
    # be false.
    leagues = [lg["name"] for lg in index["economyLeagues"]]
    save("poe2/ninja-index-state.json", {"economyLeagues": [{"name": n} for n in leagues]})
    league = next(n for n in leagues if not n.startswith(("HC ", "Hardcore")) and n != "Standard")
    q = urllib.parse.quote(league)

    ex = get(f"{NINJA2}/economy/exchange/current/overview?league={q}&type=Currency")
    save("poe2/ninja-exchange-currency.json", {
        "core": slim(ex["core"]) | {"items": [slim(i) for i in ex["core"].get("items", [])]},
        "lines": [slim(line) for line in ex["lines"] if line["id"] in CURRENCIES2],
        "items": [slim(i) for i in ex["items"] if i["id"] in CURRENCIES2],
    })

    # Uniques: one on two bases (plain and Runemastered), and one listed once.
    ov = get(f"{NINJA2}/economy/stash/current/item/overview?league={q}&type=UniqueArmours")
    keep = [line for line in ov["lines"] if line["name"] in ("Black Sun Crest", "Atziri's Step")]
    save("poe2/ninja-unique-armours.json", {
        "core": slim(ov["core"]) | {"items": [slim(i) for i in ov["core"].get("items", [])]},
        "lines": [slim(line, mods=True) for line in keep],
    })

    static = get(f"{TRADE2}/data/static")
    save("poe2/trade-static.json", {"result": [
        {"id": g["id"], "label": g.get("label"), "entries": [e for e in g["entries"] if e["id"] in CURRENCIES2]}
        for g in static["result"] if g["id"] == "Currency"
    ]})
    stats = get(f"{TRADE2}/data/stats")
    save("poe2/trade-stats.json", {"result": [
        {"id": g["id"], "label": g["label"], "entries": [e for e in g["entries"] if e["text"] in STAT_TEXTS2]}
        for g in stats["result"]
    ]})
    filters = get(f"{TRADE2}/data/filters")
    save("poe2/trade-filters.json", {"result": [
        {"id": g["id"], "filters": [{"id": f["id"]} for f in g["filters"]]} for g in filters["result"]
    ]})


if __name__ == "__main__":
    import sys
    if "--poe2" not in sys.argv:
        main()
    main_poe2()

// Which game the layer is in: Path of Exile or Path of Exile 2.
//
// POE_WAYTRADE_GAME (poe1 | poe2, set by the `waytrade` launcher) wins;
// otherwise Steam's app id for the running game (SteamAppId, SteamGameId)
// tells. Each game has its own daemon (daemon/src/game.rs), on its own
// socket, and its own small currency unit below the Divine Orb.
#pragma once

#include <cstdlib>
#include <cstring>
#include <initializer_list>

enum class Game { Poe1, Poe2 };

inline Game current_game() {
    static const Game game = [] {
        const char* set = getenv("POE_WAYTRADE_GAME");
        if (set && *set) return strcmp(set, "poe2") == 0 || strcmp(set, "2") == 0 ? Game::Poe2 : Game::Poe1;
        for (const char* var : {"SteamAppId", "SteamGameId"}) {
            const char* id = getenv(var);
            if (id && strcmp(id, "2694490") == 0) return Game::Poe2;
        }
        return Game::Poe1;
    }();
    return game;
}

inline bool is_poe2() { return current_game() == Game::Poe2; }

inline const char* game_name() { return is_poe2() ? "poe2" : "poe1"; }

// The daemon's abstract socket (daemon/src/game.rs: Game::socket_name).
inline const char* game_socket_name() { return is_poe2() ? "poe-waytrade-poe2" : "poe-waytrade"; }

// Prices under a Divine Orb: Chaos Orbs in PoE1, Exalted Orbs in PoE2. The
// trade site's currency id (and icon), its name, and the short suffix.
inline const char* small_unit_id() { return is_poe2() ? "exalted" : "chaos"; }
inline const char* small_unit_name() { return is_poe2() ? "Exalted Orb" : "Chaos Orb"; }
inline const char* small_unit_suffix() { return is_poe2() ? "ex" : "c"; }

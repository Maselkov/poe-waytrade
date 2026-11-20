#include "assets.h"

static const unsigned char inter_regular[] = {
#embed "fonts/Inter-Regular.ttf"
};
static const unsigned char inter_medium[] = {
#embed "fonts/Inter-Medium.ttf"
};
static const unsigned char cinzel[] = {
#embed "fonts/Cinzel.ttf"
};
static const unsigned char theme_rcss[] = {
#embed "ui/theme.rcss"
};
static const unsigned char price_check_rml[] = {
#embed "ui/price_check.rml"
};
static const unsigned char map_check_rml[] = {
#embed "ui/map_check.rml"
};
static const unsigned char debug_rml[] = {
#embed "ui/debug.rml"
};
static const unsigned char settings_rml[] = {
#embed "ui/settings.rml"
};
static const unsigned char toast_rml[] = {
#embed "ui/toast.rml"
};
static const unsigned char gear_png[] = {
#embed "ui/gear.png"
};

const struct EmbeddedFile embedded_files[] = {
    {"fonts/Inter-Regular.ttf", inter_regular, sizeof inter_regular},
    {"fonts/Inter-Medium.ttf", inter_medium, sizeof inter_medium},
    {"fonts/Cinzel.ttf", cinzel, sizeof cinzel},
    {"ui/theme.rcss", theme_rcss, sizeof theme_rcss},
    {"ui/price_check.rml", price_check_rml, sizeof price_check_rml},
    {"ui/map_check.rml", map_check_rml, sizeof map_check_rml},
    {"ui/debug.rml", debug_rml, sizeof debug_rml},
    {"ui/settings.rml", settings_rml, sizeof settings_rml},
    {"ui/toast.rml", toast_rml, sizeof toast_rml},
    {"ui/gear.png", gear_png, sizeof gear_png},
};
const size_t embedded_file_count = sizeof embedded_files / sizeof embedded_files[0];

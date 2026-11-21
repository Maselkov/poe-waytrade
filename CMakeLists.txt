cmake_minimum_required(VERSION 3.20)
project(poe_waytrade C CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_VISIBILITY_PRESET hidden)
set(CMAKE_C_VISIBILITY_PRESET hidden)
set(CMAKE_VISIBILITY_INLINES_HIDDEN ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE RelWithDebInfo)
endif()

find_package(PkgConfig REQUIRED)
pkg_check_modules(WAYLAND REQUIRED IMPORTED_TARGET wayland-client)
find_path(VULKAN_INCLUDE_DIR vulkan/vk_layer.h REQUIRED)

# Client code for Wayland protocols outside the core one.
pkg_get_variable(WAYLAND_PROTOCOLS_DIR wayland-protocols pkgdatadir)
find_program(WAYLAND_SCANNER wayland-scanner REQUIRED)
set(PROTOCOL_DIR ${CMAKE_BINARY_DIR}/protocols)
set(PROTOCOL_SOURCES)
# ext-data-control: setting the clipboard without an input serial (to restore
# the user's clipboard after our copy); wayland-protocols >= 1.39.
# wlr-layer-shell (vendored in protocols/, wlroots' protocol): taking the
# keyboard while typing into a panel (wl_grab.cpp); it refers to xdg-shell.
foreach(xml ${WAYLAND_PROTOCOLS_DIR}/staging/fractional-scale/fractional-scale-v1.xml
            ${WAYLAND_PROTOCOLS_DIR}/staging/ext-data-control/ext-data-control-v1.xml
            ${WAYLAND_PROTOCOLS_DIR}/stable/xdg-shell/xdg-shell.xml
            ${CMAKE_SOURCE_DIR}/protocols/wlr-layer-shell-unstable-v1.xml)
  get_filename_component(name ${xml} NAME_WE)
  set(header ${PROTOCOL_DIR}/${name}-client-protocol.h)
  set(code ${PROTOCOL_DIR}/${name}-protocol.c)
  add_custom_command(
    OUTPUT ${header} ${code}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${PROTOCOL_DIR}
    COMMAND ${WAYLAND_SCANNER} client-header ${xml} ${header}
    COMMAND ${WAYLAND_SCANNER} private-code ${xml} ${code}
    DEPENDS ${xml})
  list(APPEND PROTOCOL_SOURCES ${header} ${code})
endforeach()

# The build's version (version.h), from `git describe` at every build; the
# PKGBUILD's pkgver when there's no git. See cmake/version.cmake.
file(STRINGS ${CMAKE_SOURCE_DIR}/packaging/arch/PKGBUILD PKGVER_LINE REGEX "^pkgver=")
string(REGEX REPLACE "^pkgver=" "" PKGVER "${PKGVER_LINE}")
set(VERSION_DIR ${CMAKE_BINARY_DIR}/version)
add_custom_target(poe_waytrade_version
  COMMAND ${CMAKE_COMMAND} -DSRC=${CMAKE_SOURCE_DIR} -DOUT=${VERSION_DIR}/version.h -DFALLBACK=${PKGVER}
          -P ${CMAKE_SOURCE_DIR}/cmake/version.cmake
  BYPRODUCTS ${VERSION_DIR}/version.h
  COMMENT "Checking the build version")

# SPIR-V for the UI renderer, as comma-separated words to #include.
find_program(GLSLC glslc REQUIRED)
set(SHADER_DIR ${CMAKE_BINARY_DIR}/shaders)
set(SHADER_OUTPUTS)
foreach(shader rml.vert rml.frag track.comp)
  set(out ${SHADER_DIR}/${shader}.inc)
  add_custom_command(
    OUTPUT ${out}
    COMMAND ${CMAKE_COMMAND} -E make_directory ${SHADER_DIR}
    COMMAND ${GLSLC} -O -mfmt=num -o ${out} ${CMAKE_SOURCE_DIR}/src/shaders/${shader}
    DEPENDS ${CMAKE_SOURCE_DIR}/src/shaders/${shader})
  list(APPEND SHADER_OUTPUTS ${out})
endforeach()

# RmlUi core (third_party/rmlui, 6.3), built static with the FreeType font
# engine. Its own CMake setup wants samples and installs headers, so it's
# built from here instead.
pkg_check_modules(FREETYPE REQUIRED IMPORTED_TARGET freetype2)
file(GLOB_RECURSE RMLUI_SOURCES CONFIGURE_DEPENDS ${CMAKE_SOURCE_DIR}/third_party/rmlui/Source/Core/*.cpp)
add_library(rmlui_core STATIC ${RMLUI_SOURCES})
target_include_directories(rmlui_core PUBLIC ${CMAKE_SOURCE_DIR}/third_party/rmlui/Include)
target_compile_definitions(rmlui_core PUBLIC RMLUI_STATIC_LIB
                           PRIVATE RMLUI_FONT_ENGINE_FREETYPE RMLUI_VERSION="6.3")
target_link_libraries(rmlui_core PRIVATE PkgConfig::FREETYPE)

add_library(VkLayer_poe_waytrade SHARED
  src/layer.cpp
  src/overlay.cpp
  src/rml_renderer.cpp
  src/ui.cpp
  src/icons.cpp
  src/assets.c
  src/wl_input.cpp
  src/wl_pointer.cpp
  src/wl_grab.cpp
  src/uinput_kbd.cpp
  src/item_text.cpp
  src/price_check.cpp
  src/screen_scan.cpp
  src/gpu_track.cpp
  src/settings_page.cpp
  src/stash_scroll.cpp
  src/hotkeys.cpp
  src/daemon_client.cpp
  ${PROTOCOL_SOURCES}
  ${SHADER_OUTPUTS}
)
target_include_directories(VkLayer_poe_waytrade PRIVATE ${VULKAN_INCLUDE_DIR} ${PROTOCOL_DIR} ${SHADER_DIR}
  ${VERSION_DIR} ${CMAKE_SOURCE_DIR}/third_party/stb)
add_dependencies(VkLayer_poe_waytrade poe_waytrade_version)
target_compile_definitions(VkLayer_poe_waytrade PRIVATE VK_NO_PROTOTYPES VK_USE_PLATFORM_WAYLAND_KHR)
# Fonts and UI documents are built in with #embed (C23).
set_source_files_properties(src/assets.c PROPERTIES
  COMPILE_OPTIONS "-std=gnu23;--embed-dir=${CMAKE_SOURCE_DIR}/assets"
  OBJECT_DEPENDS "${CMAKE_SOURCE_DIR}/assets/ui/theme.rcss;${CMAKE_SOURCE_DIR}/assets/ui/price_check.rml;${CMAKE_SOURCE_DIR}/assets/ui/map_check.rml;${CMAKE_SOURCE_DIR}/assets/ui/debug.rml;${CMAKE_SOURCE_DIR}/assets/ui/settings.rml;${CMAKE_SOURCE_DIR}/assets/ui/toast.rml;${CMAKE_SOURCE_DIR}/assets/ui/scan_labels.rml;${CMAKE_SOURCE_DIR}/assets/ui/gear.png")
pkg_check_modules(WAYLAND_CURSOR REQUIRED IMPORTED_TARGET wayland-cursor)
target_link_libraries(VkLayer_poe_waytrade PRIVATE PkgConfig::WAYLAND PkgConfig::WAYLAND_CURSOR rmlui_core)
# The layer is loaded into the game (inside Steam's runtime container), so keep
# its dependencies minimal and don't leak symbols like RmlUi's into the process.
target_link_options(VkLayer_poe_waytrade PRIVATE
  -static-libstdc++ -static-libgcc
  -Wl,--exclude-libs,ALL
  -Wl,--no-undefined
  -Wl,--version-script=${CMAKE_SOURCE_DIR}/src/layer.map
)
set_property(TARGET VkLayer_poe_waytrade APPEND PROPERTY LINK_DEPENDS ${CMAKE_SOURCE_DIR}/src/layer.map)

include(GNUInstallDirs)

# Manifest for testing from the build tree:
#   VK_ADD_IMPLICIT_LAYER_PATH=build/layer ENABLE_POE_WAYTRADE=1 vkcube --wsi wayland
set(LAYER_PATH $<TARGET_FILE:VkLayer_poe_waytrade>)
configure_file(VkLayer_poe_waytrade.json.in ${CMAKE_BINARY_DIR}/layer.json.in @ONLY)
file(GENERATE OUTPUT ${CMAKE_BINARY_DIR}/layer/VkLayer_poe_waytrade.json
     INPUT ${CMAKE_BINARY_DIR}/layer.json.in)

# Installed manifest points at the installed library.
set(LAYER_PATH ${CMAKE_INSTALL_FULL_LIBDIR}/libVkLayer_poe_waytrade.so)
configure_file(VkLayer_poe_waytrade.json.in ${CMAKE_BINARY_DIR}/install/VkLayer_poe_waytrade.json @ONLY)

install(TARGETS VkLayer_poe_waytrade LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR})
# Steam launch-option wrapper: `waytrade %command%`.
install(PROGRAMS packaging/waytrade DESTINATION ${CMAKE_INSTALL_BINDIR})
install(FILES ${CMAKE_BINARY_DIR}/install/VkLayer_poe_waytrade.json
        DESTINATION ${CMAKE_INSTALL_DATADIR}/vulkan/implicit_layer.d)

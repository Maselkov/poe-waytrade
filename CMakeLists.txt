cmake_minimum_required(VERSION 3.20)
project(poe_overlay CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_VISIBILITY_PRESET hidden)
set(CMAKE_VISIBILITY_INLINES_HIDDEN ON)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
if(NOT CMAKE_BUILD_TYPE)
  set(CMAKE_BUILD_TYPE RelWithDebInfo)
endif()

find_package(PkgConfig REQUIRED)
pkg_check_modules(WAYLAND REQUIRED IMPORTED_TARGET wayland-client)
find_path(VULKAN_INCLUDE_DIR vulkan/vk_layer.h REQUIRED)

set(IMGUI_DIR ${CMAKE_SOURCE_DIR}/third_party/imgui)

add_library(VkLayer_poe_overlay SHARED
  src/layer.cpp
  src/overlay.cpp
  src/wl_input.cpp
  src/uinput_kbd.cpp
  src/item_text.cpp
  src/price_check.cpp
  ${IMGUI_DIR}/imgui.cpp
  ${IMGUI_DIR}/imgui_draw.cpp
  ${IMGUI_DIR}/imgui_tables.cpp
  ${IMGUI_DIR}/imgui_widgets.cpp
  ${IMGUI_DIR}/backends/imgui_impl_vulkan.cpp
)
target_include_directories(VkLayer_poe_overlay PRIVATE ${IMGUI_DIR} ${VULKAN_INCLUDE_DIR})
target_compile_definitions(VkLayer_poe_overlay PRIVATE
  VK_NO_PROTOTYPES
  IMGUI_IMPL_VULKAN_NO_PROTOTYPES
  IMGUI_DISABLE_DEMO_WINDOWS
  IMGUI_DISABLE_DEBUG_TOOLS
)
target_link_libraries(VkLayer_poe_overlay PRIVATE PkgConfig::WAYLAND)
# The layer is loaded into the game (inside Steam's runtime container), so keep
# its dependencies minimal and don't leak symbols like ImGui's into the process.
target_link_options(VkLayer_poe_overlay PRIVATE
  -static-libstdc++ -static-libgcc
  -Wl,--exclude-libs,ALL
  -Wl,--no-undefined
)

include(GNUInstallDirs)

# Manifest for testing from the build tree:
#   VK_ADD_IMPLICIT_LAYER_PATH=build/layer ENABLE_POE_OVERLAY=1 vkcube --wsi wayland
set(LAYER_PATH $<TARGET_FILE:VkLayer_poe_overlay>)
configure_file(VkLayer_poe_overlay.json.in ${CMAKE_BINARY_DIR}/layer.json.in @ONLY)
file(GENERATE OUTPUT ${CMAKE_BINARY_DIR}/layer/VkLayer_poe_overlay.json
     INPUT ${CMAKE_BINARY_DIR}/layer.json.in)

# Installed manifest points at the installed library.
set(LAYER_PATH ${CMAKE_INSTALL_FULL_LIBDIR}/libVkLayer_poe_overlay.so)
configure_file(VkLayer_poe_overlay.json.in ${CMAKE_BINARY_DIR}/install/VkLayer_poe_overlay.json @ONLY)

install(TARGETS VkLayer_poe_overlay LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR})
install(FILES ${CMAKE_BINARY_DIR}/install/VkLayer_poe_overlay.json
        DESTINATION ${CMAKE_INSTALL_DATADIR}/vulkan/implicit_layer.d)

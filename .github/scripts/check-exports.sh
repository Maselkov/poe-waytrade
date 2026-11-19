#!/bin/sh
# The layer is loaded into the game's process: it must export the Vulkan
# loader's entry points and nothing else (see src/layer.map).
set -eu
lib=${1:-build/libVkLayer_poe_waytrade.so}
got=$(nm -D --defined-only "$lib" | awk '{print $3}' | sort)
want=$(printf '%s\n' vkGetDeviceProcAddr vkGetInstanceProcAddr vkNegotiateLoaderLayerInterfaceVersion)
if [ "$got" != "$want" ]; then
  echo "Unexpected exports from $lib:" >&2
  echo "$got" >&2
  exit 1
fi
echo "Exports OK: $(echo $got)"

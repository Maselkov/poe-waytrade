# Writes version.h with POE_WAYTRADE_VERSION, the build's version from
# `git describe`, the same way daemon/build.rs does for the daemon: "0.21.0" on
# a release tag, "0.21.0-5-gcf90910" after it, "-dirty" for uncommitted
# changes; without a tag, FALLBACK plus the commit; without git, FALLBACK.
# WAYTRADE_VERSION in the environment overrides it.
#
# Run at every build (cmake -P) by the poe_waytrade_version target. The file is
# only rewritten when the version changes, so it doesn't force a rebuild.
#   cmake -DSRC=<repo> -DOUT=<header> -DFALLBACK=<pkgver> -P version.cmake

if(DEFINED ENV{WAYTRADE_VERSION} AND NOT "$ENV{WAYTRADE_VERSION}" STREQUAL "")
  set(version "$ENV{WAYTRADE_VERSION}")
else()
  set(version "${FALLBACK}")
  find_package(Git QUIET)
  if(GIT_FOUND)
    execute_process(
      COMMAND ${GIT_EXECUTABLE} describe --tags --match "v[0-9]*" --dirty --always
      WORKING_DIRECTORY ${SRC}
      RESULT_VARIABLE result OUTPUT_VARIABLE out ERROR_QUIET OUTPUT_STRIP_TRAILING_WHITESPACE)
    if(result EQUAL 0 AND NOT out STREQUAL "")
      if(out MATCHES "^v(.*)$")
        set(version "${CMAKE_MATCH_1}")
      else()
        # --always gave just the commit: no release tag in the history yet.
        set(version "${FALLBACK}-g${out}")
      endif()
    endif()
  endif()
endif()

set(content "#pragma once\n#define POE_WAYTRADE_VERSION \"${version}\"\n")
if(EXISTS ${OUT})
  file(READ ${OUT} old)
endif()
if(NOT "${old}" STREQUAL "${content}")
  file(WRITE ${OUT} "${content}")
endif()

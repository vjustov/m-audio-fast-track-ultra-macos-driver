# HAL plugin for the M-Audio Fast Track Ultra. Builds FastTrackUltra.driver.
# Based on libASPL's example template (third_party/libASPL/examples/_template).
cmake_minimum_required(VERSION 3.12.0)
project(FastTrackUltra CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(CMAKE_OSX_ARCHITECTURES "arm64")

# Ad-hoc signature by default ("-"). Set to a Developer ID to sign for distribution.
set(CODESIGN_ID "-" CACHE STRING "Codesign ID")

set(DRIVER_NAME "FastTrackUltra")
set(DRIVER_VERSION "0.1.0")
set(DRIVER_COPYRIGHT "Personal build")
set(DRIVER_IDENTIFIER "local.ftu.plugin")
set(DRIVER_UID "3B9809A0-C299-4DD0-8D8B-C7CF8B05D669")
set(DRIVER_ENTRYPOINT "FtuEntryPoint")

get_filename_component(LIBASPL_SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}/../third_party/libASPL ABSOLUTE)

include(ExternalProject)
ExternalProject_Add(libASPL_ext
  SOURCE_DIR ${LIBASPL_SOURCE_DIR}
  BINARY_DIR ${CMAKE_CURRENT_BINARY_DIR}/libASPL-build
  INSTALL_DIR ${CMAKE_CURRENT_BINARY_DIR}/libASPL-prefix
  CMAKE_ARGS -DCMAKE_INSTALL_PREFIX=<INSTALL_DIR> -DCMAKE_OSX_ARCHITECTURES=arm64 -DCMAKE_BUILD_TYPE=Release
  LOG_CONFIGURE YES
  LOG_BUILD YES
  LOG_INSTALL YES
  )

add_library(${DRIVER_NAME} MODULE src/Driver.cpp)
add_dependencies(${DRIVER_NAME} libASPL_ext)
target_include_directories(${DRIVER_NAME} PRIVATE
  ${CMAKE_CURRENT_BINARY_DIR}/libASPL-prefix/include
  ${CMAKE_CURRENT_LIST_DIR}/../shared
  )
target_compile_options(${DRIVER_NAME} PRIVATE -Wall -Wextra -Wno-unused-parameter)
find_library(LIB_CoreFoundation CoreFoundation REQUIRED)
target_link_libraries(${DRIVER_NAME} PRIVATE
  ${CMAKE_CURRENT_BINARY_DIR}/libASPL-prefix/lib/libASPL.a
  ${LIB_CoreFoundation}
  )

set_target_properties(${DRIVER_NAME} PROPERTIES
  OUTPUT_NAME "${DRIVER_NAME}"
  BUNDLE true
  BUNDLE_EXTENSION "driver"
  PREFIX ""
  SUFFIX ""
  MACOSX_BUNDLE ON
  MACOSX_BUNDLE_INFO_PLIST "${CMAKE_CURRENT_SOURCE_DIR}/Info.plist.in"
  MACOSX_BUNDLE_BUNDLE_NAME "${DRIVER_NAME}"
  MACOSX_BUNDLE_BUNDLE_VERSION "${DRIVER_VERSION}"
  MACOSX_BUNDLE_COPYRIGHT "${DRIVER_COPYRIGHT}"
  MACOSX_BUNDLE_GUI_IDENTIFIER "${DRIVER_IDENTIFIER}"
  MACOSX_BUNDLE_SHORT_VERSION_STRING "${DRIVER_VERSION}"
  )

add_custom_command(TARGET ${DRIVER_NAME} POST_BUILD
  COMMENT "Signing ${DRIVER_NAME}.driver with ID ${CODESIGN_ID}"
  VERBATIM
  COMMAND codesign --force -s "${CODESIGN_ID}" "${CMAKE_CURRENT_BINARY_DIR}/${DRIVER_NAME}.driver"
  )

# DaemonLink against a fake daemon (no device, no launchd service). make test runs it.
add_executable(daemon_link_tests tests/DaemonLinkTests.cpp)
target_include_directories(daemon_link_tests PRIVATE src ${CMAKE_CURRENT_LIST_DIR}/../shared)
target_compile_options(daemon_link_tests PRIVATE -Wall -Wextra -Wno-unused-parameter -fblocks)

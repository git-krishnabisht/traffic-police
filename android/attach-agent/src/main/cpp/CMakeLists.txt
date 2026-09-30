cmake_minimum_required(VERSION 3.22.1)
project(trafficpolice_agent LANGUAGES C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

set(SLICER_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../../third_party/slicer")
set(SLICER_SOURCES
    ${SLICER_DIR}/bytecode_encoder.cc
    ${SLICER_DIR}/code_ir.cc
    ${SLICER_DIR}/common.cc
    ${SLICER_DIR}/control_flow_graph.cc
    ${SLICER_DIR}/debuginfo_encoder.cc
    ${SLICER_DIR}/dex_bytecode.cc
    ${SLICER_DIR}/dex_format.cc
    ${SLICER_DIR}/dex_ir.cc
    ${SLICER_DIR}/dex_ir_builder.cc
    ${SLICER_DIR}/dex_utf8.cc
    ${SLICER_DIR}/instrumentation.cc
    ${SLICER_DIR}/reader.cc
    ${SLICER_DIR}/tryblocks_encoder.cc
    ${SLICER_DIR}/writer.cc
)

add_library(trafficpolice_agent SHARED agent.cpp ${SLICER_SOURCES})
target_include_directories(trafficpolice_agent PRIVATE
    "${CMAKE_CURRENT_SOURCE_DIR}/include"
    "${SLICER_DIR}/export"
)
target_compile_options(trafficpolice_agent PRIVATE
    -Wall -Wextra -Werror
    -Wno-sign-compare
    -Wno-unused-parameter
    -Wno-shift-count-overflow
    -Wno-missing-braces
)
target_link_options(trafficpolice_agent PRIVATE "-Wl,-z,max-page-size=16384")
target_link_libraries(trafficpolice_agent PRIVATE android log z)
set_target_properties(trafficpolice_agent PROPERTIES OUTPUT_NAME "trafficpolice_agent")

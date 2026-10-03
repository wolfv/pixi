cmake_minimum_required(VERSION 3.16)
project(greet-cli C)
include(GNUInstallDirs)

find_path(GREET_INCLUDE_DIR greet.h REQUIRED)
find_library(GREET_LIBRARY greet REQUIRED)

set(GREET_CLI_SUFFIX "" CACHE STRING "printed after the greeting")
add_executable(greet-cli src/main.c)
target_compile_definitions(greet-cli PRIVATE GREET_CLI_SUFFIX="${GREET_CLI_SUFFIX}")
target_include_directories(greet-cli PRIVATE ${GREET_INCLUDE_DIR})
target_link_libraries(greet-cli PRIVATE ${GREET_LIBRARY})
install(TARGETS greet-cli RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})

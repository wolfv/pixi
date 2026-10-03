cmake_minimum_required(VERSION 3.16)
project(libgreet C)
include(GNUInstallDirs)
enable_testing()

add_library(greet SHARED src/greet.c)
target_include_directories(greet PUBLIC
  $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
  $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
set_target_properties(greet PROPERTIES VERSION 0.1.0 SOVERSION 0 WINDOWS_EXPORT_ALL_SYMBOLS ON)

add_executable(test_greet tests/test_greet.c)
target_link_libraries(test_greet PRIVATE greet)
# generated/greet_version.h comes from the `codegen` build step (pixi.toml).
target_include_directories(test_greet PRIVATE ${CMAKE_CURRENT_SOURCE_DIR}/generated)
add_test(NAME greet_hello COMMAND test_greet hello)
add_test(NAME greet_empty COMMAND test_greet empty)
add_test(NAME greet_version COMMAND test_greet version)

install(TARGETS greet
  RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
  LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
  ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR})
install(FILES include/greet.h DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})

cmake_minimum_required(VERSION 3.16)
project(hello-steps C)
add_executable(hello-steps hello.c)
install(TARGETS hello-steps RUNTIME DESTINATION bin)

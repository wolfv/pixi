cmake_minimum_required(VERSION 3.8)
project(pendulum_report CXX)

find_package(ament_cmake REQUIRED)
find_package(rclcpp REQUIRED)
find_package(pendulum_msgs REQUIRED)

add_executable(pendulum_report src/pendulum_report.cpp)
target_compile_features(pendulum_report PRIVATE cxx_std_17)
ament_target_dependencies(pendulum_report rclcpp pendulum_msgs)
install(TARGETS pendulum_report DESTINATION lib/${PROJECT_NAME})

ament_package()

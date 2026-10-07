cmake_minimum_required(VERSION 3.20)
project(Canvas VERSION 1.2 LANGUAGES C CXX CUDA)

include(GNUInstallDirs)
find_package(Threads REQUIRED)
add_subdirectory(third_party/fmt)

option(CANVAS_BUILD_TESTS "Build the tests" ON)
set(CANVAS_SOURCES src/canvas.cpp src/shape.cpp)

function(canvas_add_test name)
  add_executable(${name} tests/${name}.cpp)
  target_link_libraries(${name} PRIVATE canvas)
  canvas_enable_warnings(${name})
endfunction()

macro(canvas_enable_warnings target)
  target_compile_options(${target} PRIVATE -Wall)
endmacro()

add_library(canvas STATIC ${CANVAS_SOURCES})

if(CANVAS_BUILD_TESTS)
  enable_testing()
  canvas_add_test(shape_test)
endif()

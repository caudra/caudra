#include <cuda_runtime.h>

#include "scale.cuh"

__constant__ float kScaleTable[64];

namespace scale {

__device__ __forceinline__ float scaled(float value, int slot) {
    return value * kScaleTable[slot % 64];
}

template <int BLOCK>
__global__ void __launch_bounds__(BLOCK) scale_rows(const float* input, float* output, int count) {
    __shared__ float tile[BLOCK];
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    if (index < count) {
        tile[threadIdx.x] = scaled(input[index], index);
        output[index] = tile[threadIdx.x];
    }
}

__global__ void clear_rows(float* output, int count) {
    const int index = blockIdx.x * blockDim.x + threadIdx.x;
    if (index < count) {
        output[index] = 0.0f;
    }
}

void launch_scale(const float* input, float* output, int count, cudaStream_t stream) {
    const dim3 grid((count + 255) / 256);
    clear_rows<<<grid, 256, 0, stream>>>(output, count);
    scale_rows<256><<<grid, 256, 0, stream>>>(input, output, count);
    cudaStreamSynchronize(stream);
}

}  // namespace scale

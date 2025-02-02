typedef unsigned char uint8;
typedef unsigned short uint16;
typedef unsigned int uint;
typedef unsigned long long int uint64;
typedef long long int int64;

extern "C" __global__ void kernel(float output_0[], uint info[]) {

  int threadIdxGlobal = threadIdx.x + threadIdx.y * blockDim.x +
                        threadIdx.z * (blockDim.x * blockDim.y);

  int warpSizeChecked = min(warpSize, blockDim.x * blockDim.y * blockDim.z);
  float l_0_0;
  float l_0_1;
  bool l_0_2;
  uint l_0_3;
  l_0_3 = info[uint(0)];
  l_0_0 = (threadIdxGlobal < l_0_3) ? output_0[threadIdxGlobal] : float(0);

  l_0_1 = l_0_0;
  {
    for (int offset = 1; offset < warpSizeChecked; offset *= 2) {
      l_0_1 += __shfl_xor_sync(-1, l_0_1, offset);
    }
  }
  l_0_2 = threadIdxGlobal == uint(0);
  if (l_0_2) {
    uint l_1_0;
    bool l_1_1;
    l_1_0 = info[uint(0)];
    l_1_1 = uint(0) < l_1_0;
    if (l_1_1) {
      output_0[uint(0)] = l_0_1;
    }
  }
}

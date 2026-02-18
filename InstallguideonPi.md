sudo apt install libssl-dev pkg-config
sudo apt install libfontconfig1-dev libfreetype6-dev
sudo apt install llvm-dev libclang-dev clang
sudo apt install libgphoto2-dev libusb-1.0-0-dev libexif-dev



# Install indi-drivers : 

```bash
sudo apt-get install -y \
  git \
  cdbs \
  dkms \
  cmake \
  ninja-build \
  fxload \
  libev-dev \
  libgps-dev \
  libgsl-dev \
  libraw-dev \
  libusb-dev \
  zlib1g-dev \
  libftdi-dev \
  libjpeg-dev \
  libkrb5-dev \
  libnova-dev \
  libtiff-dev \
  libfftw3-dev \
  librtlsdr-dev \
  libcfitsio-dev \
  libgphoto2-dev \
  build-essential \
  libusb-1.0-0-dev \
  libdc1394-dev \
  libboost-regex-dev \
  libcurl4-gnutls-dev \
  libtheora-dev \
  libxisf-dev


```
mkdir -p ~/Projects
cd ~/Projects
git clone --depth 1 https://github.com/indilib/indi.git
cd ~/Projects/indi
cmake -B build -G Ninja -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_BUILD_TYPE=Debug
cmake --build build
sudo cmake --install build
sudo apt install indi-eqmod

# Install dnglab
git clone https://github.com/dnglab/dnglab.git
cd dnglab
cargo build --release

# Lauch indiserver:

indiserver indi_eqmod_telescope
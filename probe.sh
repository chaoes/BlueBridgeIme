sudo apt-get update -y >/dev/null 2>&1
sudo apt-get install -y file nsis >/dev/null 2>&1
file /usr/bin/makensis.exe /usr/bin/makensis /usr/bin/nsis /usr/bin/makensis-compress 2>&1 || true
ls -la /usr/lib/nsis /usr/share/nsis 2>&1 | head -30
md5sum /usr/bin/makensis.exe /usr/bin/makensis 2>&1
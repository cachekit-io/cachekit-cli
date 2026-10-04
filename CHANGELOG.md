# Changelog

## [0.2.0](https://github.com/cachekit-io/cachekit-cli/compare/v0.1.0...v0.2.0) (2026-10-04)


### Features

* **run:** CK_LOG=debug prints one stderr line per call (LAB-8070) ([#17](https://github.com/cachekit-io/cachekit-cli/issues/17)) ([04c27d3](https://github.com/cachekit-io/cachekit-cli/commit/04c27d3266aceaa742fc22ac4b91e0f2333ff9a5))

## 0.1.0 (2026-10-03)


### Features

* **run:** ck run --backend saas (LAB-5119) ([#5](https://github.com/cachekit-io/cachekit-cli/issues/5)) ([6f72b9a](https://github.com/cachekit-io/cachekit-cli/commit/6f72b9ad01cfeef85f12c2b3d26c0076ca193cf3))
* **run:** ck run on the file backend (LAB-5118) ([04d6376](https://github.com/cachekit-io/cachekit-cli/commit/04d637670c77ae7b3f83b604d5c068c6f316b78d))
* **run:** ck run on the file backend (LAB-5118) ([a363524](https://github.com/cachekit-io/cachekit-cli/commit/a3635245e9d4f80ca44a5569ab2b4a51e1a3ec23))
* **run:** give the command an empty stdin, so no call needs &lt; /dev/null (LAB-7678) ([d392201](https://github.com/cachekit-io/cachekit-cli/commit/d3922013654bf189a6fbc563636c90dab4aeb686))
* **run:** give the command an empty stdin, so no call needs &lt; /dev/null (LAB-7678) ([4bb6417](https://github.com/cachekit-io/cachekit-cli/commit/4bb6417f285582465ff0041c0f07330edd1e6a46))


### Bug Fixes

* **deps:** require cachekit-rs 0.9 for the hit-path budget (LAB-5118) ([eae24bb](https://github.com/cachekit-io/cachekit-cli/commit/eae24bb42d13f2f95248a0ae1d46154739980e69))
* **run:** a received signal wins over a spawn failure (LAB-5118) ([f6a5266](https://github.com/cachekit-io/cachekit-cli/commit/f6a52665a34cef43af253983630cfaab05138147))
* **run:** answer a received signal before serving without a command (LAB-5118) ([c0ab398](https://github.com/cachekit-io/cachekit-cli/commit/c0ab3984b8ded8db4f6ac7ed390639f5bebf38ee))
* **run:** decide interruption from ck's own signals only (LAB-5118) ([11e0d48](https://github.com/cachekit-io/cachekit-cli/commit/11e0d481e8f4ac64e032ed18e492e7e1066f963b))
* **run:** hold handled signals while installing handlers (LAB-5118) ([681862e](https://github.com/cachekit-io/cachekit-cli/commit/681862e89fa810812eaa2123825b70bfe103e188))
* **run:** install the iterator handlers before the flag handlers (LAB-5118) ([37a45dd](https://github.com/cachekit-io/cachekit-cli/commit/37a45dd2b29febdbdda23cc4d3a0868ef835b0a6))
* **run:** keep forwarding signals after the first (LAB-5118) ([12932f4](https://github.com/cachekit-io/cachekit-cli/commit/12932f443afccf70b9e201ccf5e0a4805ce638fd))
* **run:** signal, marker and stdin edge cases (LAB-5118) ([2e1b2f5](https://github.com/cachekit-io/cachekit-cli/commit/2e1b2f507e76e407358051f015b3ba11b82b98bb))
* **run:** start one waiter, not one per repeated signal (LAB-5118) ([9b58452](https://github.com/cachekit-io/cachekit-cli/commit/9b584524a2e030cc2b37c018cca5b85466d0a7fb))
* **run:** survive an inherited ignored SIGCHLD (LAB-5118) ([c126a69](https://github.com/cachekit-io/cachekit-cli/commit/c126a69109e8cf52f7ba9778b9a341c310a09cc4))
* **run:** wait for the command, not its output, on a signal (LAB-5118) ([4995883](https://github.com/cachekit-io/cachekit-cli/commit/4995883d1bcb072d076bdf7bd88eddb1ead020bd))
* **store:** report an entry that fails to decrypt (LAB-5118) ([8a85943](https://github.com/cachekit-io/cachekit-cli/commit/8a859430021587c77de8417c6472adb22a5be0bd))

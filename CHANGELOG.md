# Changelog

## [0.2.0](https://github.com/ifdotpy/layr-studio/compare/v0.1.0...v0.2.0) (2026-10-31)


### Features

* provide stream bootstrap identity ([654989f](https://github.com/ifdotpy/layr-studio/commit/654989f68df32e9de95ae081720fd550ae3f111f))
* **runtime:** add typed agent permission contract ([d99c229](https://github.com/ifdotpy/layr-studio/commit/d99c229768e417c21b53e68d64d3b52812e64358))
* **runtime:** stream bounded draft baselines ([f1fdf1d](https://github.com/ifdotpy/layr-studio/commit/f1fdf1d1c225e7a027bf93d3dc266b967c72667f))
* **web:** accept stream bootstrap sessions ([dfc589b](https://github.com/ifdotpy/layr-studio/commit/dfc589b0eb637fabc59b9830dcab0a6f9d085b3a))
* **web:** add independent screen route selectors ([22ad92a](https://github.com/ifdotpy/layr-studio/commit/22ad92afc73a86e2e6fa92851b98003d3b2c43bc))
* **web:** define chat selection URLs ([263794f](https://github.com/ifdotpy/layr-studio/commit/263794f7c593593d35885fbf6ae018645d36f93f))
* **web:** deliver draft snapshots in stream order ([ac7b523](https://github.com/ifdotpy/layr-studio/commit/ac7b523acf8da4e1ad359acfc0aaa7a18888c79f))


### Bug Fixes

* **runtime:** align closed record schemas and fixtures ([f0946ac](https://github.com/ifdotpy/layr-studio/commit/f0946ac68567dd7a9eade0976c0394c42ba223b5))
* **runtime:** align item index and retire capture receipts ([150ddab](https://github.com/ifdotpy/layr-studio/commit/150ddab6c8ee08105eb176f8d5e2950f8602a7ff))
* **runtime:** align persisted agent and outbox checks ([d8bc464](https://github.com/ifdotpy/layr-studio/commit/d8bc46461a1b637d1c0fdd7d9cb9c9c76aa244a1))
* **runtime:** authorize stream progress resources ([9417351](https://github.com/ifdotpy/layr-studio/commit/94173518cf22f25b71455b69b482c595a367e948))
* **runtime:** batch retained auth progress ([48944d3](https://github.com/ifdotpy/layr-studio/commit/48944d3c823ed326ff93c9d2330ee77ec193d1d8))
* **runtime:** list unsupported Codex permission modes explicitly ([e95b9aa](https://github.com/ifdotpy/layr-studio/commit/e95b9aa3aa51c5229e8e90f449e791c97469a3ad))
* **runtime:** own peer settings and overflow receipts ([fd11ad5](https://github.com/ifdotpy/layr-studio/commit/fd11ad568aa924a9ec6c2eb07652f6fce6ed88d0))
* **runtime:** preserve frozen permission receipt compatibility ([9083dee](https://github.com/ifdotpy/layr-studio/commit/9083dee0d967c0a3c3bce9c9b38fd1d5aca67178))
* **runtime:** restore analytics revision schema on upgrade ([231a001](https://github.com/ifdotpy/layr-studio/commit/231a001af502f2cf195b507dfb6575a45a2efb01))
* **runtime:** restore newly created project records ([1533b0e](https://github.com/ifdotpy/layr-studio/commit/1533b0e3c5900925373a9237136452c508878d40))
* **runtime:** stream rename progress without interests ([7cc9824](https://github.com/ifdotpy/layr-studio/commit/7cc98247dc0919b424c480af1c4232161567570b))
* **runtime:** stream retained auth progress ([13b9758](https://github.com/ifdotpy/layr-studio/commit/13b975853f79ec2cd7f3f54e933512bd230e63c1))
* **runtime:** stream retained vault job progress ([54b1cb1](https://github.com/ifdotpy/layr-studio/commit/54b1cb11df7d680179a7d8d09331a9f0a95fe519))
* **web:** avoid replaying preferences on projection ([ff263db](https://github.com/ifdotpy/layr-studio/commit/ff263db32760e5e02738334e570d26026969c1c9))
* **web:** consume read selection on navigation ([1f33233](https://github.com/ifdotpy/layr-studio/commit/1f332330f251e36e0a89f411959d486cbe9c0c64))
* **web:** identify push subscriptions without local storage ([8a435d0](https://github.com/ifdotpy/layr-studio/commit/8a435d025371a2ee5a005b72a272dceb72038392))
* **web:** mark chats read only after selection ([f97f7c4](https://github.com/ifdotpy/layr-studio/commit/f97f7c4b3e5a8a66d75075f3316ab128c5025326))
* **web:** open one sync stream per tab ([15f0aae](https://github.com/ifdotpy/layr-studio/commit/15f0aaee3d7ffb0b5fb9dc27d3508aab8bf86b09))
* **web:** pair discovered servers only on request ([8e67cdb](https://github.com/ifdotpy/layr-studio/commit/8e67cdb6a94319d5f085852f1bacb8dd2b30c6d4))
* **web:** persist request identities before use ([5f52366](https://github.com/ifdotpy/layr-studio/commit/5f523661441ec079bb30f568df1a6a3c85442473))
* **web:** preserve complaint response identities ([7c94812](https://github.com/ifdotpy/layr-studio/commit/7c948121eb71152e252b0016915d3177f6cf448a))
* **web:** preserve concurrency fences during cleanup ([8eddf61](https://github.com/ifdotpy/layr-studio/commit/8eddf617f3ce60e6d24d0dc22489c7b0b180dcdd))
* **web:** preserve preference edits before projection ([263cd77](https://github.com/ifdotpy/layr-studio/commit/263cd775f3fcce7f3837be2916cd0578b386b681))
* **web:** preserve uncertain remote send outcomes ([711e910](https://github.com/ifdotpy/layr-studio/commit/711e9106d434dcf7797de6dc9ba7890e605430e3))
* **web:** remove retired automatic access storage ([edfdd94](https://github.com/ifdotpy/layr-studio/commit/edfdd9426009b15a1f7aadf70317a4ebb79ba269))
* **web:** remove retired draft and upload copies ([6047716](https://github.com/ifdotpy/layr-studio/commit/604771669c84dd2aa713566dd84ba0413ff80439))
* **web:** remove retired routes and pairing data ([8e9fb82](https://github.com/ifdotpy/layr-studio/commit/8e9fb821b6ba0e102b61da847dd16ec56c7369b1))
* **web:** require explicit server pairing for accounts ([f43e05d](https://github.com/ifdotpy/layr-studio/commit/f43e05dc12a35ed69f6bcd9e26000ddad9ca2638))
* **web:** send messages without the replication outbox ([6bd6af7](https://github.com/ifdotpy/layr-studio/commit/6bd6af72fc0243a490cef9736d9d35fd7c13a234))
* **web:** update alias after workspace identity arrives ([f6516c4](https://github.com/ifdotpy/layr-studio/commit/f6516c4c165026d2ede66e18cf231fe0d25ffcb0))
* **web:** use streamed discovery snapshots ([a263ae0](https://github.com/ifdotpy/layr-studio/commit/a263ae0eedc38bc5fa6254f9c098d0b556e3a71c))
* **web:** wire stream session identity ([ef1b66b](https://github.com/ifdotpy/layr-studio/commit/ef1b66b7e88b44d7e8a15367be07932653080635))


### Performance

* **runtime:** derive strict start attempt decoding ([22fb6ba](https://github.com/ifdotpy/layr-studio/commit/22fb6baafd96b48b5cb65cdb019403fb03ce1c2c))
* **runtime:** skip unchanged transcript agent reads ([fe96f81](https://github.com/ifdotpy/layr-studio/commit/fe96f816dde6103676215d02868a1ef199842a35))
* **tooling:** key gate results on tree content and rerun only failed steps ([#719](https://github.com/ifdotpy/layr-studio/issues/719)) ([826bc85](https://github.com/ifdotpy/layr-studio/commit/826bc858f2fb9d77a86db536a1bdd0e76f38f62f))
* **tooling:** link test dependencies dynamically and merge integration test binaries ([#712](https://github.com/ifdotpy/layr-studio/issues/712)) ([a02389e](https://github.com/ifdotpy/layr-studio/commit/a02389e0d6905551817ae57e0e02b10c89fc1569))
* **web:** avoid rendering retained history rows ([f5c869f](https://github.com/ifdotpy/layr-studio/commit/f5c869f53b8c1acac3f09f470260e4490073c2e7))
* **web:** isolate composer from streaming text ([a5e34ce](https://github.com/ifdotpy/layr-studio/commit/a5e34cef452dce67f4acd3cb8ff8fb7559919b08))
* **web:** simplify transcript scroll updates ([5526c5f](https://github.com/ifdotpy/layr-studio/commit/5526c5f8a7e3755477e3700ab60c397013a6bdc3))


### Reverts

* 661 (provider queue server) from the integration ([8bf41d5](https://github.com/ifdotpy/layr-studio/commit/8bf41d5f93050e4083313a97eca9ae3fd8c775b0))

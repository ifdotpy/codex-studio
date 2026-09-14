"""Apply the reviewed limit fixes without restarting active Studio work.

Python 3.14 admits the exact ccbd147, 7415ada, f496684, a50ac70 and 7f77579 implementations. Existing functions,
callbacks, HTTP closure cells, tool lists and runtime objects keep their identity.
Schema changes run lazily through future normal calls, never during this patch.
"""
import ast
import copy
import hashlib
import importlib.util
from pathlib import Path
import sys
from types import CodeType, FunctionType, MethodType, ModuleType

from codex_active_task_update import signature
from codex_progress_update import _http_closure, digest
from codex_resource_removal_update import _find_handler

BASE_COMMIT = 'ccbd147'
# BEGIN REVIEWED MANIFEST
HELPER_BASELINES = {'codex_context_repair': {'7f77579': '59e23131f78a9147be0419e0bcae329e7b982e1ae8db65ed475ac5661a7283f5',
                          '7415ada': '200a8196052edd3ced721d8ddfe7511d51a60da180e851e3f2af53fff365ac1f',
                          'a50ac70': '9eaf690e24068685f0ca61a754101515a4b842fe20dae2d01f053e6fb235ddfe',
                          'f496684': 'ccab4526509a2f72e1965444d538b604c8431f305f28a3a7d2da5671e30138f0'}}
HELPER_UPGRADES = {'codex_context_repair._callback_barrier': (None,
                                            'df178aeab1b6d277e4dd28ebe213683423a0cbb495a7a2ff59eff9058359dde0'),
 'codex_context_repair._cleanup_source': (None,
                                          '30cb9e12dc6d08a6df8bd30c4d08b0652c0f9d63a49aa168cc07dfd3c3226792'),
 'codex_context_repair._current': ('8ffec061ba070c3603608fda09fbe91add5a3a977610b975b45dced80aeb3d6e',
                                   '3732b846818729cc7a31c8af381936b0117a5420ce4db0fe76dd7c1d394c5ca6'),
 'codex_context_repair._defer_context': (None,
                                         'a1d4ad93c604aeaae405bd2523877b99a68558343fbbe5e91a872acba7e5f60a'),
 'codex_context_repair._local_idle': ('93da65b90f9a4b69260118663a8aa0c226a4e26dd9475fd2588045d0e6fe4595',
                                      'd29466729b24a95874f7bb1fb04c17a430292455013b57dee112b5987eab8c77',
                                      '66d114d58b4b157e4aa5d743c48da6e4a100d6e584bee2c21f5613caa9fb9767'),
 'codex_context_repair._native_idle': ('d1a23cf97c939f599747203a6fcdcdc72bc7f54f5483a8752129a6f0d4665d90',
                                       '9a9678fbef3dc3e8c84d70c1023020ad27adfd5e125eaac9058c2e630a7a91ea',
                                       'd51c58eecb0438ed25d9fa2282016fff195d0d8ca53e4aa25941a666f0b1f932'),
 'codex_context_repair._native_items': (None,
                                        '6b8f9fbe0b2b0b9ed7fac075228d7bda0fc494968ab08e3b6b833293cf73eeb3'),
 'codex_context_repair._native_read': (None,
                                       'bf4c5caae273a4f00731c13dc3f5419700e45d60b2365e15ee2b74cb532530e9'),
 'codex_context_repair._repair': ('c614f8410795993560b296e2b1472f1e120620a7aa0efe4de507cecaf077d511',
                                  '7cd0eb16f58e2d88b8874516912d79a360f0677bf0772c807620ace90f8d81b5',
                                  '779b57a3ef375f19281be417234d85b7e123881ae1b96886378bc6e8be96e47c',
                                  'ea2e1d89d62d0b6c7b785a4e0f88ef321281a75a7e163a680829685869639d08'),
 'codex_context_repair._settle': ('48cd337f3654a6c301cf72a09131e65cd119bd444d4e456691e1bbf8a15a170b',
                                  'a70cd4e4d7b44cd94a374e1292ca2447061b4e14792813e0aa735a16773396b5'),
 'codex_context_repair._terminal_native_item': (None,
                                                '1c25e1e4d33ed8d9d44d31f2204af9b092b93e9d13f35b7fb1f84e96b2bd3596'),
 'codex_context_repair._unresolved_tool_receipts': (None,
                                                    '9e35ab9e749c6e237acfdc7de802117e37a37aff20e3e7087998035c0c51766a'),
 'codex_context_repair._unsettled_inputs': (None,
                                            '3b3edc0700da4ff1c9c4e36ac51578d1460974927a5334871e9944f35849c48d'),
 'codex_context_repair._unsubmitted': (None,
                                       'cc91cb6045b9e3351683147bee5be3307cd4d755f045402a9d1d99e7ea506749'),
 'codex_context_repair._waiting': (None, 'e33bc9ce1b84654646b78c891d34d421595a5d3238c067a5b45bd681bedf7355'),
 'codex_context_repair.assert_context_available': ('73d6a4282aac1e927b0b7ef9ca0fe38c348b309b64e5127451eedc2537027c37',
                                                   '418d603a2431e748ea663591e311f4bb7212be4ea92aba6a8b3126bb347e5c58'),
 'codex_context_repair.blocked': ('90fc20953e813d3c023e8c2b06ae0d8b3dd8c1a965e5bf799137a873d4b57182',
                                  '9f69686706d049fea0e9cb369d62813b05b17f50ec6cf61cc7d6a2e8269bac81'),
 'codex_context_repair.claim_context_wait': (None,
                                             'fc10f342d54e135355f452026b968789f0d1e321da2152cfe45568e3891fce6c'),
 'codex_context_repair.defer_context_start': (None,
                                              '3986f18f49d1acb3fb05b60f1f8b3d399b41bab3d95c663bbd714181678abd3e'),
 'codex_context_repair.recover_context_failures': (None,
                                                   '8340e5e37ee3c3cdf6525b91b02c2e099769a9a548125244cf6f23455663f138',
                                                   '2ac7d3b3715debf1fa2bcb1bf4a990753d42d130004ad89f0ff03265a2d82987'),
 'codex_context_repair.repair_before_start': ('a17e5d717d5b75610ad80a14691a415ee923e6a0f81766e191f4bf0628076157',
                                              '8ea452c0c46969b54cd04b81b458fa1ddbdc57b271abf7d767e0deb59597d0b8'),
 'codex_context_repair.sanitized_rollout': ('8eeaf9cda2fa8020627ab9b6e188b2af99f0fe12ebbfe57fe8f478db6f65ac1e',
                                            'f634364cd9bf36650babe05a16fe82ebbe26fb2992abb48ab636f1f86c9f6646',
                                            'c301b951d82a8fbef79158bc4550e0faf136237c5904f5d9a076d2826538db4a')}
EXPECTED = {'codex_account_transfer.AccountTransfers.local_blocker': ('8db9b680aadd73c306fe2ecc90ad092152c06fd72044b5d9ab7c18dbd40a8c43',
                                                           '1c5eda98a0fe8fd99c6686b176de3f420f7ee439e54a1e36bddfb9f03c5c0ffa'),
 'codex_agent_modes.tool_mode_context': ('c28b40bac6cad47c525c9f6fd9dc749e0cd346ebacfdb6f5d3c934a164faa442',
                                         '449cb231dcc7fbdd978de4b989a6af1f39ab44cd567fd2a93cd27681f183b8dc'),
 'codex_analytics.AnalyticsMixin.analytics_event': ('50559fdd52cb5744e933792156d23a369c259055a8b838f7386eac1dab918432',
                                                    '7435557a0dde87aca83f85fa1303712c3e9897e161b12280a0b372094d393e48'),
 'codex_analytics.AnalyticsMixin.analytics_init': ('b98226c3cb96ba4e6910fc55c3318b7541f9b8d293500f6d3d2e7e76a898bc52',
                                                   '75878b56fb3f5211baeb934fd8e845799b9c3738f9d822172cb765bdc23e35e4'),
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_ensure_running': (None,
                                                                                    'd6240ac26f8c5bda0f54be70508e763f366758b1fcc91e7157f055bdfcdf7cef'),
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_init': ('89bff3f0110e6dad34ac8cd2f51a7834db0ca5a1d6338da3182e64b97dc62cf0',
                                                                          '53911a9ab69c7fa2b679a7d3c94710800e7075e3e7307bfbfa4b94e1b02f2cde'),
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_start': ('9d03976c5b43d9e29b03545f8d5bb3331ee2819347f365f429f81e39f115c3d4',
                                                                           '3f12a4186591f17db12c9604575d77b994eb19811e53500887e9c6e82fe3f612'),
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_step': ('a38a0d225cd2de7774c1fdfb5f1096d96ef8e42025cfce11f3b613d2a780bede',
                                                                          'd9bf5ce48fd24be4c80b9ccb51a4c676dca4c8a727560265f865b2edb26f1c6e',
                                                                          'ab84f87911161b69a065be595978eddace26be009be410934e68d2b04363defb'),
 'codex_analytics_history._history_worker_error': (None,
                                                   '5516bb5224ce6219b82f8f3fc4893f42c7d286b964126e8bab2a99a6fc8cd55a'),
 'codex_analytics_history._history_worker_state': (None,
                                                   '0ddf8231c8a22285a56de4ae4fb73724f77b51570ce0b0cd8b38f990980c3f38'),
 'codex_analytics_history.inherited_usage_threads': (None,
                                                     '870310c042f3a103f1bc7bd61bd26e3a77ed45ab1d34339a94b3e996b8af68b6'),
 'codex_analytics_history.repair_terminal_errors': (None,
                                                    '96a582c95c676364909d8bfc40d1b4398c25a2e353dbe72aba76afd65e02aae8'),
 'codex_analytics_history.rollout_actions': ('4938a456e379a205c84e1731a8aa3af7c4f611a87bd656cbc3bf43a8dbc3b854',
                                             'bf72b84b6615a0657a7b93dc0cdf79fbab7c2240eaa2b56317262c5a7233e6d9'),
 'codex_browser_recovery.busy': ('f11dc453f996da882fca2ed4926fc1b360b290586ef54548cc416e12382b389d',
                                 '781d88d97def6e217ba790687a183aabb0e249d6b41e20b4df34d76eb4e068f2'),
 'codex_canvas.make_server.Handler.do_POST': ('decb0105e2be4dc46614eb8112f7b3f8b99484d6e53920de2bba1cf275338771',
                                              'd5c6d1ae17c0c3a07d8017dd6b03914c8df9eae076ae24be2e530a3b5c1e776c'),
 'codex_chat_reviews._feedback_item': (None,
                                       '4b2b17eb434ecbe3a27d084af40340f94a920d3b9cf7eb7b83042df7dc9099c2'),
 'codex_chat_reviews._metadata': (None, '09e75ddefdf8d825eab2ea19c0cdc0d26225257c7bb9f7eb4a5de89f707f272a'),
 'codex_chat_reviews._prompt': ('116f9c9941976e930ddb8fe4efbc9971e06e873c8d97a96f2bcf80bc13298dc9',
                                '3bb746fd3fb4b7bda9143e1c97d294633445305fab1f71b45b1b2df5bafec04f'),
 'codex_chat_reviews._save_metadata': (None,
                                       'f1d89b49057824e8162ad846379363f62665020c2a02f3818326b97ffd5b3be2'),
 'codex_chat_reviews._snapshot': ('b3a38ed85c824acd432ff984cf17ac65a1e81abd47df580d57d6934968cacc6c',
                                  '166190816468b9243ae2ba62d8433cc7b7899cc2b3aafc2bca3138027818f7c4'),
 'codex_chat_reviews.message_review': (None,
                                       '1b42ae6c0c8d99ecb4b9ca2d0a2f5ff80802261dcff9a55ed28dc8ebd28e846d'),
 'codex_chat_reviews.parent_review': (None,
                                      '92c90703a42fbe75537846dd9e1214da4d9816f8d5a6af1d23f46b47d7982cf1'),
 'codex_chat_reviews.record_review_message': (None,
                                              '787d29815e922251e118027907caada22e0d2dd189b05cd4d35b16961b03b92a'),
 'codex_chat_reviews.review_tick': ('0eca3737cdf1bc7edd26c4b33bc7be5f8f38fd64f1d0ef4fdc697bec4f353e26',
                                    '992a45d7b495abb450d54afb494e34ad70bc0c11c977d5d3e3aa1aec2b447579'),
 'codex_chat_reviews.review_turn': (None, '95554e40c2a3ea3ebd0b722c964990f9d6407cc51ee1cec5c79e1bce3787dff7'),
 'codex_efficiency.EfficiencyMixin.bounded_event': (None,
                                                    '784d1639424220f8a495d84142baf8497a3193fe7d3d113bc03492da931adc27'),
 'codex_efficiency.EfficiencyMixin.confirm_model_tool_result': (None,
                                                                '6aee0f8fe966a2af33921c19f6b2b4836c4777287def5234e91d1576602d904d'),
 'codex_efficiency.EfficiencyMixin.model_chat_page': (None,
                                                      'aa6e1c7969b24dfe81ff85fb15a04a3f860a09a08d713404628c28cbd7842ba1'),
 'codex_efficiency.EfficiencyMixin.model_directory': ('c331c087dbb76f0172b7b917aba4b0f7dee65907eb966f6e15531207137690e3',
                                                      'a10332be1d9e9849d9bfc5ef19d1d59b6bc3a647e7e5619e1a5ba5c3d0c11c5f'),
 'codex_efficiency.EfficiencyMixin.model_event_text': ('e1683e6b6578a60e0945a1e37c9cfd5dbd8a3052ada03b8cf89c6882884eb493',
                                                       '5a10406561119bc092bb28d82f67d86576ebe0e9f3c09ecd4f8c4eea8d4f7396'),
 'codex_efficiency.EfficiencyMixin.model_known_context': (None,
                                                          '86c1e9a511a47a01f790eaa71e9955691d58097e4febd5ffb18e9cce2995a7fb'),
 'codex_efficiency.EfficiencyMixin.model_page': ('a646751369979f0db9b95d0d6f1f866af998d20bbcc48fc939885b1f0ca9e348',
                                                 'e95e9915acfd7ff3e82a0f1ef3402c32ae8393050a870a1fedbbbe539f5f5daa'),
 'codex_efficiency.EfficiencyMixin.model_read': ('aa77227f26abf16c21590ad51473da8eb6864b374b5e331f66dea475d2588903',
                                                 '44a229e95eaba22456949ec81c7d87737ed25f2f5c6a34942dfe16cd3c48e0fa'),
 'codex_efficiency.EfficiencyMixin.model_saved_message': (None,
                                                          'a96b630b00215164e45d015ea3867c41f253131f06270d9af377c38c8f135214'),
 'codex_efficiency.EfficiencyMixin.model_tool_result': ('d353f9759416e4384bf4f19438906af3dcb0f619934ae3d064360542a40c003d',
                                                        'bb8e607717dc371aca2b90656626a5230f0d408ce77c2b581d10ba7c0d1da3dd'),
 'codex_efficiency.EfficiencyMixin.model_turn_context': ('856efd3fe7d51d2009ee51d00230e61cf48e9e56ce43ab15d3b21315c935b4be',
                                                         'a0df6ba8350ecb86400beb86f6bd7e07208e3a66c061394655089e9a07221f6d'),
 'codex_efficiency.EfficiencyMixin.preparation_context_versions': (None,
                                                                   'b3145134f6e2d4b12e97a8081ec34cf2a32bb9d7b0919f3018ad2602ff15f81c'),
 'codex_efficiency.efficiency_tools': ('25e8141755dfcda03868da5eb80645159bdc79086331bbac816c69bb22504252',
                                       '0ec788db93822a304287d5a10ff831605e53ce13f1c7532ffe0cc4b02b90d947'),
 'codex_efficiency.model_text_bytes': (None,
                                       '6a1ef656c9fa819850d814120158e26556f364b9d2ba571754129b4d799c601e'),
 'codex_native_voice.NativeVoice._assert_start_context': (None,
                                                          '5705cfa41cac9743a5c47b07712a1bcad6263002cb87054f8a29dbe1b3e4808d'),
 'codex_native_voice.NativeVoice._reload_idle': ('415701653862f43d9ed80d9594b4797ac09716778e488d7315b79419f07048ca',
                                                 '9dcd02b7a60aa389682a054c4a1d46420048915a7794075103f29627f6bfe68e'),
 'codex_native_voice.NativeVoice._start_native': ('63b8d278e4ddbc9cfd5b06dc65243ed0c946f5befc4da1866dbab14392cd41c8',
                                                  '4b5bf34ffd5d489808ac54a3ede7b5492978a9179db62751b1fdd10c6ca7a940'),
 'codex_native_voice.NativeVoice._submit_start': ('9a021bc98330068130baa4e34bc9ee35dc029f7dcea5aa2ea2b4f519b38fdad1',
                                                  '0183cf56110dd5d19c7a5e6a088b1c344195f1108493a33b7185d4828f6073d3'),
 'codex_native_voice.NativeVoice.start': ('d60351599e22423821adfce00e54ca316eb0212062ffd955cc0be5e15e74f45e',
                                          'a47463dc08671e4245185dcc86f8c52f2f03d75c38ee4c67be495300fb4756db'),
 'codex_runtime.Runtime.chat_message': ('b1a3a90e723b6f4dfb5568412b066b2714c58c5ec12afb9224a767580f33fe5f',
                                        '707dce71f666f4b70dfa7e9debf5494072a00f61d2e9806266a4b487ed6cd315'),
 'codex_runtime.Runtime.chat_read': ('fa4d1b66df0e82306edae7b84cd99b22d7a376d0863a21fecb1ee89518f2bae7',
                                     '738b31dbb7ac55854e50344e729fc4293af49929272d756e0503c0458a145d2f'),
 'codex_runtime.Runtime.complaint': ('ddfe91b3098f6caefc2dd9c52da2219ef27a13d3d8d4ff6e6c6adc01f9b9610f',
                                     '3d04d12dd474a7be5eead91c68b55b5858ce5a5230769d18d49c99c1fd7394c3'),
 'codex_runtime.Runtime.dispatch': ('d7bb2dfa7d7597681ca39a591b4dd02120db63dabb4ab674f04a3834be798347',
                                    '14cdf04f11da7b34cb39d462d0fab05744417634a79e79643b94d8e9c8f81505',
                                    '11ca91182f2554ab678e4660c3b887d079e22ff45c924441008d39ec72c31850',
                                    '5663ae51be5b9f938dde85042da686224986109e8304e2950cdcfb2b10b9ebf8'),
 'codex_runtime.Runtime.dynamic': ('a343c4822dc25a246fe4ad0c2bffb287b3941b11b84cd18361367a558968e3b1',
                                   '282a4c1fda1e179c4dd95aa0fb3dc2384d91bc0afd8c4984decc3ecb7732225c'),
 'codex_runtime.Runtime.native_action': ('71c665aa2afec153428aaff002d00a722738acc1c30d2fbd4cf663b7e8e0529c',
                                         '42962aa254033dd505a2fa1bc1394fef051bcb34a0169bb90353fe9af28d27cf'),
 'codex_runtime.Runtime.new_thread_params': ('657c5c181919e3cfeacb2667169d5019a2417092d0691cf12ffc4a2842a53ef3',
                                             'e162c895e862bd4a9befa87d3fc9d4e09fc3b1593883bb22474ce21b3b07c607'),
 'codex_runtime.Runtime.notification': ('89450ccbe330e21529ca2459c6000279b794206ac410351f85f0ec6d4d0072e4',
                                        '143d829b3d40c95ce7901653ca6e44c6c1b7ab68051be617f27784dd1da620c9'),
 'codex_runtime.Runtime.parent_event': ('5aaefac8df44704f5b43fbb2536d7bac66b92f0dfe37283059c7e8914c463949',
                                        'c7d671907bf3bb0198f3eb805b001a39b3eed8a049b4e7a73642378cad3104f9'),
 'codex_runtime.Runtime.prepare_locked': ('2c5513a882e165921fc8c38d8686369dc32d6f3da6350d3bff1d3830c0a6c895',
                                          'aab8c519064c7136449f0351ce4682ef0161d3f3468be55ba8a9e5ecf1a05d7c'),
 'codex_runtime.Runtime.prepared_result': ('e59baed43b656e27bd3a9d1987f1879d4603bd41eff9a0c7273c9711733a450d',
                                           'fa4ddd7d4a3f6b79127e0562cc6cbf059dd152b85fd6541e58cadcedaed46144'),
 'codex_runtime.Runtime.run_native_action': ('086804757634cf287f8c57bf52238bdf0788c80a3fafa2326af838a14a3c3b84',
                                             'ec16ce03bf40b16c3ee785bb7ede2b3773612fccc3e09a8a7eb68c84250baf69',
                                             'd1838dc91719db821c24a4988c391c52c0095363859c4a14e31692169fef6dbe'),
 'codex_runtime.Runtime.send': ('62b77be1c3618219eae7ffdc070bcaf55a172ddbceb6a4d135bb9cccbfdb486d',
                                '56f5b33967163ff41b1a26d21f8a9495b10b797a1655bf57c5e6698b4a82af82'),
 'codex_runtime.Runtime.start': ('362ec61e1d70ea3d127f59627c7d7111bc6e4e26b860b7ea1d3dc023e4fbd72c',
                                 '51cadbaa97832c5563a39c3f6ad2cfe7be888a808c69cf7dd98e5eb8faf7b1ab'),
 'codex_runtime.Runtime.start_error': ('b4c32d85a96bd2d7beaf8018b3c45f9d62f4d58ef8155b431cef3f5bf1d8176a',
                                       'f7396ef483b6b65bd3cd1bce04590a5cd1813869cbd017c4209128e31a3a36aa',
                                       'f940516efa0571b05523252d24ecf12112e6064e30abe386e0ef1d3a225a741c'),
 'codex_work.WorkMixin.work_action': ('83907197198082242cf723594b4f8707b4aaa0374bc55a5cc629b40dc9203527',
                                      '1743a5dc4524f1065c71f2709e0b4182617abe8a8caa8974a8b89c1fbcc4cada'),
 'codex_workspace.WorkspaceMixin.assert_workspace_available': ('d789137b99f96a138ee944c9af5ad3ceacf1dabc8b4182caf2faf29a45446981',
                                                               '38b6a26fab0dddad5ffd735fcdc1899057b1131aa43c7e1dccba95387137f682')}
SOURCE_SHA = {'codex_account_transfer': 'd3bb785c611b95b27343d6c4aaaf1b3e811cd2dd9044feaa83efab4a74ae926a',
 'codex_agent_modes': '881bcb2985c283787b937ac06d043df51ba0ee1853063ac941502bbbc0bcba38',
 'codex_analytics': '95d31c3d6122376e08a5ca840fb8ef5dfd2a975dc4d6fc1dafb9cb9653e4720d',
 'codex_analytics_history': '370ed3699c769dbff9b32baa5a831580d7bb184e88d3d0dee7ed860495a7e352',
 'codex_browser_recovery': 'b212d954451ab92db8ab69fe06e55caaa717dc25f71c21abc2879d3284ea0fc4',
 'codex_budget': 'fb2a9a60065b7278cd4741c8326c6f06cb581950cb29632f3560dee89e5f0359',
 'codex_canvas': '6e9bb7d6d39cb61078cc6ae851aaff54ae8605c906a1d5a4574840a963608d72',
 'codex_chat_reviews': '6f7a6e2f811d5694b908b2f6447cc45d16440d38ea32db9d760e652e4a177df9',
 'codex_context_repair': 'cac82e291be01d0cf7615aa51a51f0814c47a3435db2c0c73026342320e9a445',
 'codex_efficiency': '339eb402d4c4666acda6e896e928b9abd12aef3680b0470bbacecb7c2305fecc',
 'codex_native_action_receipts': '902646dd0a63dc51a69191abbe69be2746d51e841baf530965e6d1ff8f2dd1a7',
 'codex_native_voice': '8fbff84260eff954a07552c391b42c26dc75509cce91cb6ed93659a7e2dc89d7',
 'codex_runtime': 'f6257caf9bff9f39f276de3cc7adfc63b60e16bbd03da52bfa9581dd17aecba8',
 'codex_tool_requests': 'd7a08b5539f1192b92438bddc38fd1a7f02052a80dc05b3d033705c47fd23b34',
 'codex_wakeups': 'c4e2c44925d4f7ce971e4cc0f7800df61e19fef55891b38f22d5439f6ed8fa60',
 'codex_work': 'c7f37d9d6f37115b62f790e8d89ddd1c3de99cf3c2713768e78bbc0016fdf5cb',
 'codex_workspace': '02d923a4624d996a491a4266554dcb16922d7d5984e08edee0f1667a64854673'}
NEW_MODULES = {'codex_native_action_receipts', 'codex_context_repair', 'codex_budget', 'codex_wakeups'}
OPTIONAL_MODULES = {'codex_native_voice', 'codex_browser_recovery', 'codex_agent_modes', 'codex_chat_reviews'}
GLOBAL_IMPORTS = {'codex_analytics': {'budget_capture': ('codex_budget', 'budget_capture')},
 'codex_analytics_history': {'budget_migrate': ('codex_budget', 'budget_migrate'),
                             'budget_prepare_migration': ('codex_budget', 'budget_prepare_migration')}}
CONSTANTS = {'codex_tool_requests._MESSAGE_REJECTIONS': ({'Message must have 1 to 12000 characters',
                                              'Select another agent',
                                              'Unknown managed agent',
                                              'Unknown message importance',
                                              'Versioned progress requires a progress_key and nonnegative '
                                              'integer progress_version'},
                                             {'A no-issue result requires the exact active review event and '
                                              'target',
                                              'Message must have 1 to 12000 characters',
                                              'Select another agent',
                                              'Unknown managed agent',
                                              'Unknown message importance',
                                              'Versioned progress requires a progress_key and nonnegative '
                                              'integer progress_version'})}
TOOLS_DIGEST = ('7abaeeff65bd2ceb4c5b0b786b8ce2bd47267e7e4242cdbc64113d488371277a',
 'a76bf69476cdf347a932ced5f5a6a2f0ab2c5f7abad4124f6a0b271e468ce0a4')
TOOL_CHANGES = {'orchestration_message': {'description': 'Share a finding, question, or answer with other agents during '
                                          'work. target is an agent id in your team, parent, lead, or '
                                          'broadcast (your team). Broadcasts notify only active agents; '
                                          'other recipients can read them in chat history. Private chats are '
                                          'visible to their participants and the user. Direct messages wake '
                                          'idle recipients but never resume stopped agents. Use '
                                          'importance=progress only for routine updates; these batch briefly '
                                          'and keep the latest progress per sender, room and progress_key '
                                          'when progress_version increases. Use the task id as progress_key. '
                                          'Without these fields, every update is retained. Original messages '
                                          'remain in chat history. Questions and blockers deliver '
                                          'immediately. Send when you have new information or an answer for '
                                          'the recipient.',
                           'inputSchema': {'additionalProperties': False,
                                           'properties': {'importance': {'enum': ['message',
                                                                                  'progress',
                                                                                  'question',
                                                                                  'blocker',
                                                                                  'result'],
                                                                         'type': 'string'},
                                                          'progress_key': {'type': 'string'},
                                                          'progress_version': {'minimum': 0,
                                                                               'type': 'integer'},
                                                          'review_event_id': {'type': 'string'},
                                                          'review_outcome': {'enum': ['no_issue'],
                                                                             'type': 'string'},
                                                          'target': {'type': 'string'},
                                                          'text': {'type': 'string'}},
                                           'required': ['target', 'text'],
                                           'type': 'object'},
                           'name': 'orchestration_message',
                           'type': 'function'},
 'orchestration_read': {'description': 'Read a full saved tool response or orchestration event by '
                                       'output_ref. Offsets are Unicode characters; pages are bounded. Use '
                                       'contains to locate relevant output. Same-agent records only. Never '
                                       'rerun a mutation to recover its result.',
                        'inputSchema': {'additionalProperties': False,
                                        'properties': {'contains': {'type': 'string'},
                                                       'offset': {'minimum': 0, 'type': 'integer'},
                                                       'output_ref': {'type': 'string'}},
                                        'required': ['output_ref'],
                                        'type': 'object'},
                        'name': 'orchestration_read',
                        'type': 'function'}}
CONSTRUCTOR_TRANSITIONS = {'codex_account_transfer.AccountTransfers.__init__': 'Unchanged. Existing transfer state is retained; absent '
                                                     'stores remain absent.',
 'codex_analytics.AnalyticsMixin.analytics_init': 'No replay. budget_prepare_migration creates the new scan '
                                                  'index on the next history step, outside Runtime.lock.',
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_init': 'No replay. The next worker call '
                                                                         'initializes missing schema and '
                                                                         'preserves its existing guard, '
                                                                         'cursor and path cache.',
 'codex_analytics_history.AnalyticsHistoryMixin.analytics_history_start': 'No replay. Existing live workers '
                                                                          'retain their run callback. Normal '
                                                                          'dispatch starts the new loop only '
                                                                          'when no worker is alive.',
 'codex_native_voice.NativeVoice.init_native': 'Unchanged. Existing voice connections and locks are '
                                               'retained; absent stores remain absent.',
 'codex_runtime.Runtime.__init__': 'Unchanged. Lazy helpers create budget, mode and native-action tables '
                                   'during future normal operations.'}
# END REVIEWED MANIFEST
MISSING = object()


def _literal(node):
    # One existing history-reader default uses integer multiplication.
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Mult):
        left, right = ast.literal_eval(node.left), ast.literal_eval(node.right)
        if type(left) is int and type(right) is int:
            return left * right
    return ast.literal_eval(node)


def source_function(source, path, namespace, closure=None, compiled_source=None):
    """Extract reviewed functions without executing imports or default expressions."""
    if compiled_source is None:
        node = ast.parse(source)
        compiled = compile(node, '<limit-fix-update>', 'exec', dont_inherit=True)
    else:
        node, compiled = compiled_source
    for name in path:
        nodes = [child for child in node.body
                 if isinstance(child, (ast.ClassDef, ast.FunctionDef)) and child.name == name]
        codes = [child for child in compiled.co_consts if isinstance(child, CodeType) and child.co_name == name]
        if len(nodes) != 1 or len(codes) != 1:
            raise RuntimeError('Unknown limit-fix source structure')
        node, compiled = nodes[0], codes[0]
    if not isinstance(node, ast.FunctionDef):
        raise RuntimeError('Expected a limit-fix function')
    decorators = node.decorator_list
    static = len(decorators) == 1 and isinstance(decorators[0], ast.Name) and decorators[0].id == 'staticmethod'
    if decorators and not static:
        raise RuntimeError('Unknown limit-fix function decorator')
    defaults = tuple(_literal(value) for value in node.args.defaults) or None
    if closure is None and compiled.co_freevars:
        closure = tuple((lambda value: lambda: value)(None).__closure__[0] for _ in compiled.co_freevars)
    function = FunctionType(compiled, namespace, node.name, defaults, closure)
    function.__kwdefaults__ = {arg.arg: _literal(value)
                              for arg, value in zip(node.args.kwonlyargs, node.args.kw_defaults)
                              if value is not None} or None
    return function, static


def _module(name, directory):
    module = sys.modules.get(name)
    if (type(module) is not ModuleType or module.__name__ != name
            or Path(getattr(module, '__file__', '')).resolve() != directory / (name + '.py')):
        raise RuntimeError('Unknown limit-fix module location: ' + name)
    return module


def _stage_module(name, raw, directory):
    # These exact-hash helpers contain definitions and reviewed imports only.
    # They perform no database, runtime, connection or external actions at import.
    for node in ast.parse(raw).body:
        dependencies = ([item.name for item in node.names] if isinstance(node, ast.Import)
                        else [node.module] if isinstance(node, ast.ImportFrom) else [])
        for dependency in dependencies:
            provider = sys.modules.get(dependency)
            if type(provider) is not ModuleType or provider.__name__ != dependency:
                raise RuntimeError('Unavailable limit-fix helper import: ' + str(dependency))
    path = directory / (name + '.py')
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    exec(compile(raw, str(path), 'exec', dont_inherit=True), vars(module))
    return module


def _helper_equal(live, desired):
    metadata = {'__name__', '__doc__', '__package__', '__loader__', '__spec__', '__file__', '__cached__'}
    actual = {k: v for k, v in vars(live).items() if k not in metadata}
    expected = {k: v for k, v in vars(desired).items() if k not in metadata}
    permitted_missing = {name for name in expected
                         if HELPER_UPGRADES.get(live.__name__ + '.' + name, (False,))[0] is None}
    if actual.keys() - expected.keys() or expected.keys() - actual.keys() - permitted_missing:
        raise RuntimeError('Unknown limit-fix helper globals: ' + live.__name__)
    for name, value in expected.items():
        previous = actual.get(name, MISSING)
        if isinstance(value, FunctionType) and value.__module__ == live.__name__:
            allowed = HELPER_UPGRADES.get(live.__name__ + '.' + name, (signature(value), signature(value)))
            if previous is MISSING and allowed[0] is None and signature(value) == allowed[-1]:
                continue
            valid = (signature(value) == allowed[-1]
                     and isinstance(previous, FunctionType) and previous.__globals__ is vars(live)
                     and previous.__module__ == live.__name__ and previous.__closure__ is None
                     and signature(previous) in allowed)
        elif type(value) in (str, int, float, bool, type(None), tuple, frozenset, set):
            valid = type(previous) is type(value) and previous == value
        else:
            valid = previous is value
        if not valid:
            raise RuntimeError('Unknown limit-fix helper member: ' + live.__name__ + '.' + name)


def _active_frames(originals):
    previous = {id(old[0]): target for target, _, old, desired in originals
                if old[0] is not desired.__code__}
    for frame in sys._current_frames().values():
        while frame is not None:
            target = previous.get(id(frame.f_code))
            if target is not None:
                raise RuntimeError('Limit fixes require an earlier call to finish: ' + target)
            frame = frame.f_back


def _binding(owner, name, static, instance):
    descriptor = vars(owner).get(name, MISSING)
    if descriptor is MISSING:
        live = MISSING
    else:
        if static != isinstance(descriptor, staticmethod):
            raise RuntimeError('Unknown limit-fix descriptor: ' + name)
        live = descriptor.__func__ if static else descriptor
    if instance is not None:
        classes = type(instance).__mro__
        if owner not in classes:
            raise RuntimeError('Unknown limit-fix instance owner: ' + name)
        if name in vars(instance) or any(name in vars(cls) for cls in classes[:classes.index(owner)]):
            raise RuntimeError('Unexpected limit-fix method override: ' + name)
        if live is not MISSING:
            bound = getattr(instance, name)
            if not (bound is live if static else isinstance(bound, MethodType)
                    and bound.__self__ is instance and bound.__func__ is live):
                raise RuntimeError('Unknown limit-fix method binding: ' + name)
    return live


def apply(runtime, handler_class=None):
    import codex_runtime
    if sys.version_info[:2] != (3, 14) or type(runtime) is not codex_runtime.Runtime:
        raise RuntimeError('Unknown runtime for limit fixes')
    if not EXPECTED or not SOURCE_SHA:
        raise RuntimeError('Limit-fix review manifest is incomplete')
    directory = Path(__file__).resolve().parent
    handler_class = handler_class or _find_handler(runtime)
    if (type(handler_class) is not type or handler_class.__module__ != 'codex_canvas'
            or handler_class.__qualname__ != 'make_server.<locals>.Handler'):
        raise RuntimeError('Unknown limit-fix HTTP handler')
    _http_closure(handler_class.do_GET, runtime, handler_class)
    sources = {}
    for name, expected in SOURCE_SHA.items():
        raw = (directory / (name + '.py')).read_bytes()
        if hashlib.sha256(raw).hexdigest() != expected:
            raise RuntimeError('Unreviewed limit-fix source: ' + name)
        sources[name] = raw
    modules, staged = {}, {}
    for name in SOURCE_SHA:
        if name in NEW_MODULES or name in OPTIONAL_MODULES and name not in sys.modules:
            desired = _stage_module(name, sources[name], directory)
            if name in sys.modules:
                live = _module(name, directory)
                _helper_equal(live, desired)
                modules[name] = live
            else:
                modules[name] = staged[name] = desired
        else:
            modules[name] = _module(name, directory)
    compiled_sources = {}
    for name, raw in sources.items():
        tree = ast.parse(raw)
        compiled_sources[name] = (tree, compile(tree, '<limit-fix-update>', 'exec', dont_inherit=True))
    replacements = []
    for target, allowed in (EXPECTED | HELPER_UPGRADES).items():
        name, *path = target.split('.')
        module = modules[name]
        if len(path) == 3:
            owner = handler_class
            closure = getattr(owner, path[-1]).__closure__
        else:
            owner = vars(module).get(path[0]) if len(path) == 2 else module
            closure = None
            if len(path) == 2 and (not isinstance(owner, type) or owner.__module__ != name
                                   or owner.__name__ != path[0]
                    or (owner not in type(runtime).__mro__ and (name, path[0]) not in {
                        ('codex_account_transfer', 'AccountTransfers'), ('codex_native_voice', 'NativeVoice')})):
                raise RuntimeError('Unknown limit-fix owner: ' + target)
        desired, static = source_function(sources[name], tuple(path), vars(module), closure=closure,
                                          compiled_source=compiled_sources[name])
        if signature(desired) != allowed[-1]:
            raise RuntimeError('Unreviewed limit-fix replacement: ' + target)
        replacements.append((target, module, owner, path[-1], desired, static, allowed))
    if vars(modules['codex_runtime']).get('efficiency_tools') is not vars(modules['codex_efficiency']).get('efficiency_tools'):
        raise RuntimeError('Unknown limit-fix efficiency tool alias')
    imports = []
    for name, values in GLOBAL_IMPORTS.items():
        for key, (dependency, attribute) in values.items():
            provider = modules.get(dependency) or sys.modules.get(dependency)
            if type(provider) is not ModuleType or provider.__name__ != dependency:
                raise RuntimeError('Unavailable reviewed import: ' + dependency)
            desired = getattr(provider, attribute) if attribute else provider
            previous = vars(modules[name]).get(key, MISSING)
            if previous is not MISSING and previous is not desired:
                raise RuntimeError('Unknown limit-fix import: ' + name + '.' + key)
            imports.append((modules[name], key, previous, desired))
    if not runtime.lock.acquire(timeout=10):
        raise RuntimeError('Runtime remains busy; no limit fixes applied')
    try:
        if runtime.closed:
            raise RuntimeError('Runtime is closed; no limit fixes applied')
        instances = {owner: runtime for owner in type(runtime).__mro__}
        transfer_owner = vars(modules['codex_account_transfer']).get('AccountTransfers')
        transfers = vars(runtime).get('_account_transfers')
        if transfers is not None and (type(transfers) is not transfer_owner or vars(transfers).get('rt') is not runtime):
            raise RuntimeError('Unknown live limit-fix transfer store')
        instances[transfer_owner] = transfers
        voice_owner = vars(modules['codex_native_voice']).get('NativeVoice')
        voice = vars(runtime).get('_voice_store')
        if 'codex_voice' in sys.modules:
            voice_module = _module('codex_voice', directory)
            voice_class = vars(voice_module).get('VoiceStore')
            if (not isinstance(voice_class, type) or voice_class.__module__ != 'codex_voice'
                    or voice_class.__name__ != 'VoiceStore' or voice_owner not in voice_class.__mro__
                    or vars(voice_module).get('NativeVoice') is not voice_owner):
                raise RuntimeError('Unknown live limit-fix voice owner')
        elif voice is not None:
            raise RuntimeError('Unknown live limit-fix voice module')
        if voice is not None and (type(voice) is not voice_class or vars(voice).get('runtime') is not runtime):
            raise RuntimeError('Unknown live limit-fix voice store')
        instances[voice_owner] = voice
        originals, additions = [], []
        for target, module, owner, method, desired, static, allowed in replacements:
            live = _binding(owner, method, static, instances.get(owner))
            if live is MISSING and allowed[0] is None:
                additions.append((owner, method, staticmethod(desired) if static else desired))
                continue
            if (not isinstance(live, FunctionType) or live.__globals__ is not vars(module)
                    or live.__module__ != module.__name__ or signature(live) not in allowed
                    or live.__code__.co_freevars != desired.__code__.co_freevars):
                raise RuntimeError('Unknown live limit-fix function: ' + target)
            originals.append((target, live, (live.__code__, live.__defaults__, live.__kwdefaults__), desired))
        constants = []
        for target, (old, desired) in CONSTANTS.items():
            name, key = target.split('.')
            live = vars(modules[name]).get(key)
            if (type(live) is not set or any(type(value) is not str for value in live)
                    or live != old and live != desired):
                raise RuntimeError('Unknown live limit-fix constant: ' + target)
            constants.append((live, live.copy(), desired))
        tools = codex_runtime.TOOLS
        if type(tools) is not list or digest(tools) not in TOOLS_DIGEST:
            raise RuntimeError('Unknown live limit-fix tool definitions')
        desired_tools = [copy.deepcopy(TOOL_CHANGES.get(row['name'], row)) for row in tools]
        if digest(desired_tools) != TOOLS_DIGEST[1]:
            raise RuntimeError('Unreviewed limit-fix tool definitions')
        if (not staged and not additions and all(previous is desired for _, _, previous, desired in imports)
                and all(signature(live) == signature(desired) for _, live, _, desired in originals)
                and digest(tools) == TOOLS_DIGEST[1] and all(live == desired for live, _, desired in constants)):
            return {'status': 'already_applied', 'baseCommit': BASE_COMMIT}
        updates = [entry for entry in originals if signature(entry[1]) != signature(entry[3])]
        old_tools = list(tools)
        # Refuse known active callers before exposing new helper members.
        _active_frames(updates)
        try:
            for name, module in staged.items():
                if name in sys.modules:
                    raise RuntimeError('Limit-fix helper loaded during preparation: ' + name)
                sys.modules[name] = module
            for module, key, _, desired in imports:
                setattr(module, key, desired)
            for owner, method, desired in additions:
                setattr(owner, method, desired)
            for _, live, _, desired in updates:
                live.__defaults__, live.__kwdefaults__ = desired.__defaults__, desired.__kwdefaults__
                live.__code__ = desired.__code__
            if digest(tools) != TOOLS_DIGEST[1]:
                tools[:] = desired_tools
            for live, _, desired in constants:
                live.symmetric_difference_update(live ^ desired)
            # Check after cutover so no new invocation can enter the old code.
            # A rejected active frame keeps its old code and all state untouched.
            _active_frames(updates)
        except BaseException:
            for _, live, previous, _ in updates:
                live.__code__, live.__defaults__, live.__kwdefaults__ = previous
            tools[:] = old_tools
            for live, previous, _ in constants:
                live.symmetric_difference_update(live ^ previous)
            for owner, method, _ in reversed(additions):
                if method in vars(owner):
                    delattr(owner, method)
            for module, key, previous, _ in imports:
                if previous is MISSING:
                    vars(module).pop(key, None)
                else:
                    setattr(module, key, previous)
            for name, module in staged.items():
                if sys.modules.get(name) is module:
                    del sys.modules[name]
            raise
        return {'status': 'applied', 'baseCommit': BASE_COMMIT,
                'methods': list(EXPECTED | HELPER_UPGRADES), 'helpers': sorted(NEW_MODULES),
                'constants': list(CONSTANTS), 'toolDefinitions': list(TOOL_CHANGES),
                'helperUpgrades': list(HELPER_UPGRADES),
                'imports': [name + '.' + key for name, values in GLOBAL_IMPORTS.items() for key in values],
                'constructorTransitions': CONSTRUCTOR_TRANSITIONS}
    finally:
        runtime.lock.release()

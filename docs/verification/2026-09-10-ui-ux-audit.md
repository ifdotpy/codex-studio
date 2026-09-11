# Проверка интерфейса Codex Studio относительно ChatGPT app

Дата: 10 сентября 2026 года. Базовый коммит: `cc3ab24`; проверен текущий рабочий каталог с незакоммиченными изменениями.

Найдено 52 замечания. Это все замечания, оставленные после проверки и объединения повторов, а не гарантия отсутствия других проблем.

Проверены компоненты `web/src`, их стили, маршруты в `App`, загрузка истории, черновики, голос и связанные обработчики `desktop`/`scripts`.
Проверка не является построчным аудитом всего сервера. Серверный код прочитан там, где он определяет действие интерфейса.
Исходный код приложения не изменён. Живые агенты, команды и голосовые сеансы не запускались.

**Основа сравнения**

В официальном описании ChatGPT основой остаются разговор и готовый результат. Технические подробности выделены в представление Codex. Поэтому наличие инструментов Studio само по себе не является недостатком. Вопрос в их размещении и понятности. [Use ChatGPT](https://learn.chatgpt.com/docs/use-chatgpt).

Для поиска и организации ориентиром служат отдельный поиск чатов, поиск внутри чата, проекты и архив. [Projects and chats](https://learn.chatgpt.com/docs/projects), [Commands](https://learn.chatgpt.com/docs/reference/commands).

Для голоса важно различать живой разговор и диктовку текста до отправки. Для общих предпочтений нужен постоянный раздел настроек. [Voice](https://learn.chatgpt.com/docs/features/voice), [Settings](https://learn.chatgpt.com/docs/reference/settings).

Это сравнение с опубликованными возможностями и принципами интерфейса. Конкретная сборка ChatGPT на устройстве пользователя не проверялась. Размещение функций может зависеть от платформы и версии.

**Приоритеты и доказательства**

- **P1:** сначала исправить потерю текста, неожиданное действие или неверное состояние.
- **P2:** заметное неудобство в обычном сценарии.
- **P3:** упрощение и менее частые сценарии.
- **Воспроизведено:** поведение проверено в отдельном браузере на тестовом сервере.
- **Ошибка:** вывод из конкретного обработчика или условия в коде; отдельной проверки интерфейса не было.
- **Решение:** оценка удобства. Это не обязательно ошибка реализации.
- **Пробел:** отсутствующее действие или возможность.
- **Риск:** механизм найден в коде, но реальное проявление требует проверки.

Указанный приоритет не означает, что каждое отличие нужно немедленно менять. Сначала следует решить, нужен ли Studio простой режим чата и отдельный режим управления агентами.

| № | Приоритет | Тип | Что неудобно | Предлагаемое изменение | Код |
|---|---|---|---|---|---|
| 01 | P1 | Ошибка | Tab отправляет текст вместо перехода к следующему элементу. | Сохранить обычный Tab. Для очереди дать отдельное действие. | [Conversation.tsx:925](/Users/igor/Projects/codex-agents/web/src/components/Conversation.tsx:925) |
| 02 | P1 | Ошибка | Поле ограничено 12 000 символами без счётчика и сообщения об обрезанном вводе. | Сохранить весь текст. Предложить вложение, если текст превышает предел. | [Conversation.tsx:921](/Users/igor/Projects/codex-agents/web/src/components/Conversation.tsx:921) |
| 03 | P1 | Воспроизведено | Неотправленный ответ в Messages пропадает после закрытия и повторного открытия карточки. | Сохранять черновик по идентификатору сообщения. | [UserMessages.tsx:41](/Users/igor/Projects/codex-agents/web/src/components/UserMessages.tsx:41) |
| 04 | P1 | Пробел | При обрезанной истории главного чата интерфейс сообщает, что полная история осталась на диске. Догрузить её нельзя. | Добавить страницы истории для главного чата. | [hooks.ts:155](/Users/igor/Projects/codex-agents/web/src/hooks.ts:155) |
| 05 | P1 | Воспроизведено | Пустой Plan исчезает при повторном запросе. Та же проверка loading осталась в Work, Changes, Checkpoints, Profiles и Rules. | Отделить первую загрузку от повторной проверки во всех этих разделах. | [Workspace.tsx:1451](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1451) |
| 06 | P2 | Ошибка | Refresh в Analytics удаляет отчёт и выбранную запись до получения ответа. Ошибка запроса тоже удаляет данные. | Сохранять последний отчёт при обновлении того же представления. | [Analytics.tsx:408](/Users/igor/Projects/codex-agents/web/src/components/Analytics.tsx:408) |
| 07 | P2 | Ошибка | Лимиты до первого ответа показывают Unavailable. Отдельного состояния загрузки нет. | Различать загрузку, отсутствие данных и ошибку. | [Usage.tsx:271](/Users/igor/Projects/codex-agents/web/src/components/Usage.tsx:271) |
| 08 | P2 | Решение | Третья запись инструмента заменяет две отдельные карточки закрытой группой. Открытые подробности исчезают. | Сохранять структуру группы и выбор пользователя во время ответа. | [TurnHistory.tsx:68](/Users/igor/Projects/codex-agents/web/src/components/TurnHistory.tsx:68) |
| 09 | P2 | Решение | Текст без конца предложения скрыт во время ответа. Незакрытый блок кода тоже ждёт завершения. | Показывать частичный текст и код. Отложить только обработку незавершённой диаграммы. | [sentenceStream.ts:41](/Users/igor/Projects/codex-agents/web/src/components/sentenceStream.ts:41) |
| 10 | P2 | Ошибка | Ошибка Copy source скрывает готовую диаграмму и показывает Cannot render this diagram. | Отделить ошибку копирования от ошибки построения диаграммы. | [RichPreview.tsx:199](/Users/igor/Projects/codex-agents/web/src/components/RichPreview.tsx:199) |
| 11 | P2 | Решение | Рядом с чатом постоянно видны аккаунт, проект, настройки двух типов агентов, шесть разделов и технические показатели. | Оставить главные действия. Остальные настройки открывать по запросу. | [App.tsx:1055](/Users/igor/Projects/codex-agents/web/src/App.tsx:1055) |
| 12 | P2 | Решение | Search chats ищет имя, путь и последний фрагмент. Полная история находится в другом Search. Поиск запросов видит только загруженные сообщения пользователя. | Разделить поиск по всем чатам и поиск внутри чата. Дать им ясные названия. | [Sidebar.tsx:169](/Users/igor/Projects/codex-agents/web/src/components/Sidebar.tsx:169) |
| 13 | P2 | Пробел | Open chat из результата поиска открывает чат, но не передаёт идентификатор найденного сообщения. | Переходить к точному совпадению и выделять его. | [Workspace.tsx:1397](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1397) |
| 14 | P2 | Решение | Messages открывает общую панель с 11 разделами, включая Profiles, Rules и Resources. | Отделить сообщения для пользователя от управления работой агентов. | [Workspace.tsx:66](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:66) |
| 15 | P2 | Решение | For you занимает левую колонку Messages. Пока разговор не выбран, большая правая область показывает заглушку. | Отдать сообщениям для пользователя основную область. Список командных разговоров открывать отдельно. | [team-chats.css:279](/Users/igor/Projects/codex-agents/web/src/components/team-chats.css:279) |
| 16 | P3 | Воспроизведено | Пустой Messages одновременно показывает No messages need a response, No questions or unresolved problems и три группы с нулевыми счётчиками. | Оставить одно пустое состояние. Не занимать экран пустыми группами. | [Workspace.tsx:1264](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1264) |
| 17 | P2 | Решение | Ответ на сообщение требует статуса I’ll handle it, Resolved или Declined и поля Action or reason. | Отделить обычный ответ от решения по задаче. | [UserMessages.tsx:119](/Users/igor/Projects/codex-agents/web/src/components/UserMessages.tsx:119) |
| 18 | P2 | Решение | Вопросы агента используют разные формы: карточки вариантов, выпадающий список, ответ в чате или отдельное окно. | Использовать одну форму ответа. Состояние ожидания показать текстом. | [Requests.tsx:138](/Users/igor/Projects/codex-agents/web/src/components/Requests.tsx:138) |
| 19 | P2 | Решение | Галочка задачи сразу отправляет результат агенту на проверку. После этого её нельзя снять. | Назвать действие Send for review. Не придавать ему вид обычной обратимой галочки. | [UserTasks.tsx:307](/Users/igor/Projects/codex-agents/web/src/components/UserTasks.tsx:307) |
| 20 | P2 | Воспроизведено | На экране 390 px активная вкладка Messages выходит за правую границу. Панель не прокручивает её в видимую область. | Показывать выбранный раздел полностью при открытии и смене раздела. | [Workspace.css:423](/Users/igor/Projects/codex-agents/web/src/components/Workspace.css:423) |
| 21 | P3 | Пробел | Приложение принудительно использует тёмную тему. Настройки светлой или системной темы нет. | Добавить Light, Dark и System. | [main.tsx:10](/Users/igor/Projects/codex-agents/web/src/main.tsx:10) |
| 22 | P2 | Решение | Lead, orchestrator, agent и worker обозначают участников по-разному. Work, Your tasks и Background требуют знания разных типов задач. | Использовать одни названия ролей. В названиях задач указывать, кто должен действовать. | [App.tsx:1026](/Users/igor/Projects/codex-agents/web/src/App.tsx:1026) |
| 23 | P2 | Пробел | Во время ответа нельзя подготовить модель, reasoning и Fast mode для следующего сообщения. | Разрешить отложенные настройки с явной подписью For the next turn. | [ExecutionSettings.tsx:109](/Users/igor/Projects/codex-agents/web/src/components/ExecutionSettings.tsx:109) |
| 24 | P2 | Решение | Смена модели может одновременно сбросить reasoning и выключить Fast mode. Отдельного сообщения об этих изменениях нет. | Показать, какие связанные настройки изменились. | [ExecutionSettings.tsx:137](/Users/igor/Projects/codex-agents/web/src/components/ExecutionSettings.tsx:137) |
| 25 | P1 | Решение | YOLO mode стоит рядом с Fast mode, хотя меняет разрешения всей команды. Область действия есть только в описании. | Выделить Permissions. Назвать режим по действию и явно показать область всей команды. | [ExecutionSettings.tsx:253](/Users/igor/Projects/codex-agents/web/src/components/ExecutionSettings.tsx:253) |
| 26 | P1 | Решение | Выбор аккаунта существующего чата сразу запускает перенос команды. На компьютере меню это объясняет, на телефоне поле называется Account. | Отделить выбор аккаунта нового чата от переноса команды. Перед переносом показать состав и результат операции. | [App.tsx:1423](/Users/igor/Projects/codex-agents/web/src/App.tsx:1423) |
| 27 | P2 | Ошибка | Мобильный список аккаунтов допускает выбор неготовых аккаунтов. Настольное меню блокирует их. | Использовать единые статусы и правила доступности. | [App.tsx:1431](/Users/igor/Projects/codex-agents/web/src/App.tsx:1431) |
| 28 | P2 | Решение | Интерфейс показывает глобальный Default, Project accounts и аккаунт чата. Значение набора галочек Project accounts не объяснено. | Объяснить роль набора и выбор аккаунта для нового чата. | [ProjectAccount.tsx:55](/Users/igor/Projects/codex-agents/web/src/components/ProjectAccount.tsx:55) |
| 29 | P2 | Пробел | В Manage accounts нет действия отключить аккаунт или выйти из него. | Добавить отключение с объяснением, что произойдёт с существующими чатами. | [Accounts.tsx:350](/Users/igor/Projects/codex-agents/web/src/components/Accounts.tsx:350) |
| 30 | P3 | Решение | Add account и Manage accounts открывают одно окно с одной начальной формой. | Открывать список для управления и отдельный шаг для добавления. | [Accounts.tsx:331](/Users/igor/Projects/codex-agents/web/src/components/Accounts.tsx:331) |
| 31 | P2 | Решение | Диктовка требует открыть панель, записать звук, остановить запись, запустить расшифровку и нажать Insert. | После Stop автоматически получать текст для проверки перед отправкой. | [Dictation.tsx:266](/Users/igor/Projects/codex-agents/web/src/components/Dictation.tsx:266) |
| 32 | P1 | Ошибка | Диктовка доступна в браузере, но расшифровка требует приложение macOS. Ошибка появляется только при Transcribe. | Проверять возможность расшифровки до записи. | [Dictation.tsx:198](/Users/igor/Projects/codex-agents/web/src/components/Dictation.tsx:198) |
| 33 | P2 | Пробел | На телефоне нет отдельной диктовки Studio. Доступен голосовой разговор, который может сразу передать задачу агенту. | Дать отдельный режим преобразования речи в черновик. | [Conversation.tsx:973](/Users/igor/Projects/codex-agents/web/src/components/Conversation.tsx:973) |
| 34 | P2 | Ошибка | Audio saved in this chat не говорит, что запись хранится только в IndexedDB этого браузера. | Указать On this device или обеспечить синхронизацию записи. | [Dictation.tsx:264](/Users/igor/Projects/codex-agents/web/src/components/Dictation.tsx:264) |
| 35 | P1 | Решение | Закрытие панели голоса скрывает Mute и End voice, но не завершает разговор. Снаружи остаётся только цвет значка. | Сохранять видимые статус микрофона, Mute и End voice. | [RealtimeVoice.tsx:149](/Users/igor/Projects/codex-agents/web/src/components/RealtimeVoice.tsx:149) |
| 36 | P2 | Решение | Переход в другой чат завершает голосовой разговор через удаление компонента. Отдельного сообщения об этом нет. | Явно показывать завершение или сохранять голосовой сеанс с названием исходного чата. | [RealtimeVoice.tsx:65](/Users/igor/Projects/codex-agents/web/src/components/RealtimeVoice.tsx:65) |
| 37 | P2 | Решение | Одна ошибка чтения истории голоса завершает сеанс. Автоматического восстановления этого сеанса нет. | Показать причину остановки и действие восстановления. Сохранить понятное состояние микрофона. | [RealtimeVoice.tsx:81](/Users/igor/Projects/codex-agents/web/src/components/RealtimeVoice.tsx:81) |
| 38 | P2 | Риск | При разрыве WebRTC появляется Reconnecting…, но отдельного предела ожидания после получения SDP в коде нет. | Ограничить ожидание восстановления и дать понятное действие после ошибки. | [RealtimeVoice.tsx:113](/Users/igor/Projects/codex-agents/web/src/components/RealtimeVoice.tsx:113) |
| 39 | P1 | Ошибка | В Electron настройка уведомлений начинается с false после перехода между главными чатами, хотя значение сохраняется. | Хранить настройку на уровне приложения и восстанавливать её. | [Workspace.tsx:192](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:192) |
| 40 | P2 | Пробел | Уведомление Electron не имеет обработчика нажатия. Оно не открывает нужный чат или запрос. | Связать уведомление с конкретным местом в интерфейсе. | [main.cjs:171](/Users/igor/Projects/codex-agents/desktop/main.cjs:171) |
| 41 | P2 | Решение | Крестик сеанса терминала отправляет close и может завершить запущенные процессы. Это отличается от скрытия панели. | Разделить Hide и End session. Для активного процесса явно показать результат действия. | [TerminalDock.tsx:395](/Users/igor/Projects/codex-agents/web/src/components/TerminalDock.tsx:395) |
| 42 | P2 | Ошибка | Background без явного выбора показывает filtered[0]. Новая задача может заменить текущую правую панель во время чтения. | Фиксировать первоначальный выбор или ждать выбора пользователя. | [BackgroundTasks.tsx:200](/Users/igor/Projects/codex-agents/web/src/components/BackgroundTasks.tsx:200) |
| 43 | P2 | Пробел | У обычных блоков кода нет отдельной кнопки Copy code и действия Expand. Длинный блок получает внутреннюю прокрутку. | Добавить язык, Copy code и Expand. | [StreamingText.tsx:70](/Users/igor/Projects/codex-agents/web/src/components/StreamingText.tsx:70) |
| 44 | P2 | Пробел | Markdown удаляет img. Внешняя картинка исчезает без замены, а локальные картинки обрабатываются как ссылки результата. | Использовать контролируемый просмотр изображений. Сохранять подпись при недоступном изображении. | [StreamingText.tsx:75](/Users/igor/Projects/codex-agents/web/src/components/StreamingText.tsx:75) |
| 45 | P2 | Пробел | Вложение до отправки нельзя открыть из карточки. У отправленного изображения открывает просмотр имя файла, но не сама картинка. | Открывать просмотр по карточке и изображению. | [ComposerAttachments.tsx:101](/Users/igor/Projects/codex-agents/web/src/components/ComposerAttachments.tsx:101) |
| 46 | P2 | Пробел | У отправленного сообщения нет Edit. У ответа нет Regenerate. Есть Copy, Quote и ограниченный Branch. | Добавить исправление через новую ветку. Повтор ответа должен учитывать уже выполненные действия. | [Conversation.tsx:650](/Users/igor/Projects/codex-agents/web/src/components/Conversation.tsx:650) |
| 47 | P3 | Решение | Other drafts показывает только текст. Нет времени, устройства и сравнения; Use this draft заменяет текущее поле. | Добавить происхождение версии и явные Replace или Append. | [DraftVersions.tsx:35](/Users/igor/Projects/codex-agents/web/src/components/DraftVersions.tsx:35) |
| 48 | P2 | Пробел | Долгая расшифровка показывает только Transcribing…. Нет прогресса по фрагментам и отмены. | Показать прогресс и добавить отмену с сохранением записи. | [Dictation.tsx:414](/Users/igor/Projects/codex-agents/web/src/components/Dictation.tsx:414) |
| 49 | P3 | Решение | На широком экране боковые панели нельзя свернуть. Sidebar показывается постоянно; Team постоянно видна при ширине от 1200 px. | Дать пользователю свернуть панели независимо от ширины окна. | [App.tsx:1363](/Users/igor/Projects/codex-agents/web/src/App.tsx:1363) |
| 50 | P2 | Пробел | На телефоне нельзя добавить проект. Если доступного проекта нет, приложение отправляет пользователя на Mac. | Дать понятный начальный экран и доступный способ начать чат с телефона. | [App.tsx:473](/Users/igor/Projects/codex-agents/web/src/App.tsx:473) |
| 51 | P3 | Ошибка | Recovered voice draft после Copy сохраняется под другим ключом, но исходные ключи остаются. При следующем открытии черновик возвращается. | Добавить действие завершить восстановление или скрыть этот черновик. | [RealtimeVoice.tsx:164](/Users/igor/Projects/codex-agents/web/src/components/RealtimeVoice.tsx:164) |
| 52 | P3 | Решение | Сохранённую диктовку можно удалить одним нажатием. Отмены удаления нет. | Предоставить краткое Undo для случайного удаления записи. | [Dictation.tsx:325](/Users/igor/Projects/codex-agents/web/src/components/Dictation.tsx:325) |

**Дополнительные подтверждения**

- Пункт 3: локальный `useState` хранит текст только внутри формы. При повторном открытии `setDetail(null)` удаляет форму. [UserMessages.tsx](/Users/igor/Projects/codex-agents/web/src/components/UserMessages.tsx:249).
- Пункт 5: общий таймер работает каждые пять секунд. Другие пустые состояния зависят от `!state.loading`. [Таймер](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:231), [Work](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:615), [Changes](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1127), [Checkpoints](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1533), [Profiles](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1767), [Rules](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:2052).
- Пункт 8: та же замена при третьей записи есть в [Activity.tsx](/Users/igor/Projects/codex-agents/web/src/components/Activity.tsx:385). Это компромисс между компактностью и неподвижным интерфейсом.
- Пункт 9: прямой вызов `sentencePrefix` подтверждает пустой вывод для незавершённого предложения и незакрытого блока кода. Это намеренная порционная выдача, а не потеря ответа.
- Пункт 11: технические показатели находятся в [Usage.tsx](/Users/igor/Projects/codex-agents/web/src/components/Usage.tsx:287). Кнопка Context открывает большую панель [Analytics](/Users/igor/Projects/codex-agents/web/src/components/Analytics.tsx:479).
- Пункт 12: отдельный полный поиск находится в [Workspace.tsx](/Users/igor/Projects/codex-agents/web/src/components/Workspace.tsx:1275). Поиск запросов ограничен сообщениями пользователя в [PromptNavigator.tsx](/Users/igor/Projects/codex-agents/web/src/components/PromptNavigator.tsx:22).
- Пункт 26: настольное меню честно пишет Transfer team to account. Проблема сильнее в мобильном поле Account. [Accounts.tsx](/Users/igor/Projects/codex-agents/web/src/components/Accounts.tsx:260).
- Пункт 27: настольное меню блокирует аккаунты со статусом, отличным от ready. [Accounts.tsx](/Users/igor/Projects/codex-agents/web/src/components/Accounts.tsx:273).
- Пункт 34: звук хранится локально в [dictation/storage.ts](/Users/igor/Projects/codex-agents/web/src/dictation/storage.ts:14).
- Пункт 38: вывод касается отсутствия собственного таймера после SDP. Продолжительность реального разрыва WebRTC не измерялась.
- Пункт 39: `Workspace` пересоздаётся при смене главного чата. [App.tsx](/Users/igor/Projects/codex-agents/web/src/App.tsx:1384).
- Пункт 41: сервер закрывает сеанс через SIGHUP и, при необходимости, SIGKILL. [codex_terminals.py](/Users/igor/Projects/codex-agents/scripts/codex_terminals.py:393).
- Пункт 48: запись обрабатывается фрагментами по 50 секунд. На один фрагмент предусмотрено до 120 секунд ожидания. [speech.swift](/Users/igor/Projects/codex-agents/desktop/native/speech.swift:26).
- Пункт 49: настольная Sidebar выводится без условия открытия. [Sidebar.tsx](/Users/igor/Projects/codex-agents/web/src/components/Sidebar.tsx:682).

**Проверка в браузере**

Отдельный тестовый сервер использовал временную базу. Браузер работал без видимого окна. Запросы изменения состояния были запрещены. Для проверки Plan ответ сервера задерживался на 650 мс.

- Черновик ответа в Messages исчез после закрытия карточки и повторного открытия.
- Наблюдатель за пустым Plan зафиксировал удаление элемента во время 11-секундной проверки.
- На экране 390 px вкладка Messages занимала координаты от 308,6 до 419,1 px. Правая часть оказалась за границей.
- На том же экране видны два пустых состояния и три пустые группы Messages.

[Результаты проверки](/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-ux-audit-mhl7b9/observations.json).
[Главный экран, 1440 px](/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-ux-audit-mhl7b9/chat-1440.png).
[Messages, 390 px](/var/folders/29/8pytxrvn6qlcm4384bmy9n6m0000gn/T/studio-ux-audit-mhl7b9/messages-390.png).

**Что следует сохранить**

- Разделение пользовательского терминала и команд агента.
- Точные разрешения на действия с файлами и командами.
- Различие между ошибкой отправки и неподтверждённой доставкой.
- Повтор отправки с тем же идентификатором запроса.
- Сохранение места чтения при новых сообщениях.
- Отдельные параметры новых subagents.
- Архив, папки и закреплённые чаты.
- Подробную аналитику как дополнительный инструмент.

Нельзя улучшать внешний вид ценой повторного выполнения команды, ложного подтверждения доставки или скрытия настоящей ошибки.

Исправленное ранее мигание No questions or unresolved problems в Messages не включено как действующая ошибка. Пункт 5 относится к другим разделам.

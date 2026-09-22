# Messages chat list

Messages now displays a flat conversation list with icons, search, last-message
previews, times, and locally recorded unread indicators. For you appears in the
same list. Selecting a row opens that conversation. Mobile uses a list/detail
view with a Back to chats action. The main sidebar remains unchanged.

The list initially renders 60 conversations. Search covers all available rooms.
Message order, question controls, user replies, and message text keep their
existing behavior. Personal requests and room histories cannot mix during
selection changes.

Verification on 2026-09-22:

- Production web build passed.
- `node tests/simple-messages-ui.mjs` passed: exact transcript order, room changes,
  personal-feed isolation, search, mobile navigation, and unchanged main sidebar.
- Desktop (1440 by 960) and mobile (390 by 844) screenshots were inspected.
- `node tests/desktop-notifications-ui.mjs` passed, including question navigation.
- `node tests/chat-create-latency-ui.mjs` passed against the combined interface.
- The application package uses the persistent Studio certificate.
- The installed interface reloaded through its native View > Reload menu.
  Its renderer reported healthy; backend PID 55850 remained active.
- A final signature check detected an externally added Python cache file.
  The signer now prevents writes to the bundled cache directory.
  `node desktop/signing-test.mjs` passed actual imports and strict signature
  checks across two resource versions. The installed signature passed afterward.

Browser fixtures use isolated state and do not send model requests or OS notifications.

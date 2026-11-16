# 준비와 상태 점검

처음 연결할 때는 `iph setup`, 준비 상태를 JSON으로 확인할 때는 `iph doctor`를 씁니다. `setup`은 점검 결과에 따라 사용자가 폰에서 할 일을 안내하고 다시 확인합니다.

```sh
iph setup
iph doctor
iph doctor --no-touch-test
```

`doctor`는 설정 앱을 앞으로 가져오고, 터치 확인을 위해 목록을 내렸다 올립니다. `--no-touch-test`는 목록 조작을 건너뛰지만 설정 앱 실행과 화면 읽기는 수행합니다.

## 점검 항목

| 항목 | 구분 | 확인과 처리 |
|---|---|---|
| `pairing` | 필수 | 컴퓨터에 폰의 짝짓기 기록이 있어야 합니다. 없으면 USB로 연결하고 폰에서 '이 컴퓨터를 신뢰'를 누릅니다 |
| `connection` | 필수 | USB 또는 Wi-Fi로 폰에 연결할 수 있어야 합니다 |
| `developer_mode` | 필수 | 개발자 모드 상태를 읽습니다. 꺼져 있으면 설정에 메뉴가 나타나도록 요청하고, 켜기·재시동·암호 입력은 사용자에게 안내합니다 |
| `developer_disk_image` | 필수 | 접근성·화면·입력 서비스가 제공되는지 확인하고, 없으면 개발자 디스크 이미지 설치를 시도합니다 |
| `screen_unlocked` | 필수 | lockdown의 잠금 상태와 설정 앱 실행 결과를 확인합니다. 화면 읽기 실패만으로 잠겼다고 판단하지 않습니다 |
| `settings_in_front` | 필수 | 화면은 읽히지만 설정 앱이 앞에 나오지 않았을 때 실패로 표시합니다 |
| `accessibility_tree` | 필수 | 설정 앱의 화면 요소를 읽을 수 있어야 합니다 |
| `touch_input` | 필수 | 설정 목록을 내렸다 올려 실제 움직임을 확인합니다. 움직이지 않으면 반대 방향으로도 시도합니다 |
| `wifi` | 권장 | 무선 연결 플래그와 같은 폰의 usbmuxd `Network` 엔트리를 확인합니다. 경고가 있어도 USB는 사용할 수 있습니다 |
| `auto_lock` | 안내 | 값을 읽거나 바꾸지 않고, 오래 쓸 때 자동 잠금 '안 함'을 권합니다 |
| `do_not_disturb` | 안내 | `iph dnd on`의 복원 기록이 남아 있을 때 `iph dnd off`를 안내합니다. 집중 모드를 바꾸지는 않습니다 |

UI 자동화 활성화 스위치는 직접 읽지 않습니다. 터치가 실제로 동작하는지로 판단하고, 실패하면 설정 → 개발자 → 'UI 자동화 활성화'를 켜 보도록 안내합니다.

## 개발자 디스크 이미지

필요한 서비스가 없으면 `cryptexd`로 이미지를 설치합니다. Xcode의 이미지가 있으면 우선 사용하고, 없으면 고정된 공개 저장소 리비전에서 받아 SHA-256을 확인합니다. 실패하면 Xcode의 이미지를 사용하는 image mounter 방식도 시도합니다.

이미지 다운로드·검증과 실제 폰 설치는 서로 다른 단계입니다. 자동 설치 경로는 실기기에서 검증되지 않았으므로, 실패하면 `checks[].detail`과 `user_action`을 확인하세요.

## Wi-Fi 연결

같은 Wi-Fi에 있어도 무선 발견 설정이 꺼져 있으면 폰이 보이지 않을 수 있습니다. USB로 연결한 뒤 `iph doctor`를 실행하면 `com.apple.mobile.wireless_lockdown`의 `SupportsWifi`, `SupportsWifiSyncing`, `EnableWifiConnections`, `EnableWifiDebugging`을 읽습니다.

```sh
iph setup --fix-wifi
iph doctor --fix-wifi
```

두 명령은 `--fix-wifi`가 있을 때만 무선 플래그를 바꿉니다. USB로 지원 여부가 확인되면 명시적으로 `false`인 활성화 플래그만 켜고 다시 읽습니다. 알 수 없는 값은 바꾸지 않습니다. 변경 후 같은 UDID의 `Network` 엔트리를 최대 4초 기다립니다.

- `auto_fixed: true`는 플래그가 바뀌었다는 뜻입니다. 무선 발견 성공은 `wifi.status: "ok"`로 확인합니다.
- 읽기·쓰기·시간 초과 오류는 `checks[].detail`에 담깁니다.
- 직접 설정하려면 Finder/Apple 기기 앱의 'Wi-Fi에 있을 때 이 iPhone 보기'와 Xcode의 'Connect via network'를 확인합니다.
- 두 활성화 플래그가 모두 켜져 있는데도 발견되지 않으면 같은 Wi-Fi 대역, AP/클라이언트 격리, mDNS 차단을 확인합니다. Bonjour 서비스 이름은 `_apple-mobdev2._tcp.local.`입니다.

## 결과 해석

`checks[]`의 `status`는 `ok`, `fail`, `warn`(판정 못 함), `skipped`, `info`입니다. `required: true`인 항목이 모두 `ok`여야 `ready: true`가 됩니다. `failing`에는 필수 항목 중 통과하지 못한 항목의 이름이 담깁니다.

`ok: true`는 점검 요청 자체가 끝났다는 뜻이므로 준비 여부는 `ready`로 확인합니다. `--no-touch-test`를 쓰면 필수 항목인 `touch_input`이 `skipped`가 되어 `ready: false`입니다.

알림 배너는 앱의 접근성 트리에 나오지 않아 탭을 가로챌 수 있습니다. 방해금지 모드가 필요하면 `iph dnd on`으로 켜고 작업 뒤 `iph dnd off`로 되돌립니다. [방해금지 모드 동작](how-it-works.md#7-방해금지-모드)을 참고하세요.

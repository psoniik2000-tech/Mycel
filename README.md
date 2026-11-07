# Mycel

Мессенджер без центральных серверов, устойчивый к блокировкам. Для закрытой группы до 50 человек. Работает на ПК (Windows, Linux, macOS) и телефоне (Android).

- P2P-сеть на QUIC (iroh), запасные транспорты: TLS на 443, Tor, локальная сеть, офлайн-пакеты
- Сквозное шифрование (MLS), зашифрованное локальное хранилище
- Текст, фото, видео, файлы, голосовые, видеокружки, позже — звонки
- Ядро на Rust, интерфейс на Tauri 2

Статус: работает на ПК (текст, медиа, звонки); Android в разработке. Текущее состояние и задачи — [docs/STATUS.md](docs/STATUS.md), описание — [docs/DESIGN.md](docs/DESIGN.md).

## Якорь (ПК, включённый 24/7)

Якорь — обычный участник, который хранит сообщения для тех, кто офлайн (30 дней), и держит relay для тех, кто за NAT. Публичные relay n0 не используются (включить на крайний случай: `--n0-relay`).

1. Собрать: `cargo build --release -p mycel-anchor` → `target\release\mycel-anchor.exe`.
2. На роутере пробросить TCP-порт (например 3340) на этот ПК. Узнать свой внешний IP (например, на 2ip.ru).
3. Вступить в пространство по инвайту из приложения: `mycel-anchor.exe --dir C:\MycelAnchor join mycel://join/...` (спросит пароль; затем Ctrl+C).
4. Проверить запуск: `mycel-anchor.exe --dir C:\MycelAnchor serve http://ВНЕШНИЙ-IP:3340`.
5. Автозапуск в фоне (Планировщик заданий, при включении ПК, без входа в систему), в PowerShell от администратора:

```powershell
$a = New-ScheduledTaskAction -Execute "cmd.exe" -Argument '/c set MYCEL_PASSWORD=ПАРОЛЬ&& C:\Mycel\target\release\mycel-anchor.exe --dir C:\MycelAnchor serve http://ВНЕШНИЙ-IP:3340'
Register-ScheduledTask MycelAnchor -Action $a -Trigger (New-ScheduledTaskTrigger -AtStartup) -User SYSTEM -RunLevel Highest
New-NetFirewallRule -DisplayName MycelAnchor -Direction Inbound -Protocol TCP -LocalPort 3340 -Action Allow
```

Пароль хранится в задании открытым текстом — используйте отдельный пароль только для якоря. Остановить: `Stop-ScheduledTask MycelAnchor`, удалить: `Unregister-ScheduledTask MycelAnchor`.

# Clarus totalseg demo pack — инструкция по запуску

Самодостаточное демо AI-конвейера поверх DICOMweb: **clarus** (компактный
DICOMweb PACS), **clinfer** (инференс-сайдкар), **ABI-скрипты-продюсеры** и
**clmcp** (демонстрационный Task Requester).

НЕ ДЛЯ ДИАГНОСТИКИ. Это транспорт + исследовательские модели; никаких
клинических утверждений не делается.

## Состав

- `clarus(.exe)` — однобинарный DICOMweb-сервер (QIDO/WADO/STOW/UPS-RS)
- `clinfer(.exe)` — инференс-сайдкар (забирает воркитемы, гоняет продюсеров,
  пишет результаты)
- `clarus.conf` — **дефолтный** конфиг сервера (Clarus только Task Manager)
- `task_producer.conf` — конфиг, где **Clarus сам чеканит воркитемы**
- `clinfer_demo.conf` — конфиг сайдкара под правило totalseg (минимальный)
- `clinfer.conf.example.en` — полный аннотированный конфиг сайдкара (все
  опции)
- `abi/` — примеры продюсеров (ниже)
- `ABI-README.md` — контракт продюсера (frames + manifest.json на входе,
  result.json на выходе)
- `clmcp/` — демонстрационный Task Requester (агент/MCP REPL)
- `../images/totalseg_demo.png` — результат RTSTRUCT в Weasis
- `../images/screenshot-clmcp.png` — clmcp REPL в действии

## Два режима (выберите один)

У UPS-RS три роли: **Task Manager** (всегда clarus), **Task Performer**
(всегда clinfer) и **Task Requester** — роль, которую может играть кто
угодно. В паке два конфига сервера, различающиеся ровно одной настройкой —
`task_producer`:

| Конфиг | `task_producer` | Кто создаёт воркитемы |
| --- | --- | --- |
| `clarus.conf` | `false` | внешний — clmcp, модальность, RIS |
| `task_producer.conf` | `true` | сам Clarus (авто-минт при STOW) |

### Режим A — Clarus как Task Producer (реквестер не нужен)

```
./clarus --config task_producer.conf
./clinfer --config clinfer_demo.conf
curl -X POST http://127.0.0.1:8024/dicomweb/studies \
     -H "Content-Type: application/dicom" --data-binary @slice.dcm
```

STOW → Clarus чеканит CT-воркитем → clinfer забирает → totalseg считает →
RTSTRUCT обратно. Откройте в Weasis.

### Режим B — внешний Task Requester через clmcp

```
./clarus                        # clarus.conf — дефолтный конфиг
./clinfer --config clinfer_demo.conf
python clmcp/clmcp.py --chat
```

Сначала залейте своё CT-исследование (STOW-RS, см. Режим A), затем в REPL:
`run totalsegmentator for <ваш пациент>`. clmcp создаёт воркитем по
стандартному UPS-RS (`POST /workitems`), ровно как это сделала бы
модальность или RIS.

## Конфиги — это и есть руководство

`clarus.conf` намеренно самодокументирован: каждая настройка снабжена
комментарием — что она делает, почему дефолт именно такой и когда его
менять. Читайте его сверху вниз как руководство по развёртыванию и
эксплуатации. Для этого демо важны `task_producer` (выше) и каталог
воркитемов (словарь AI-задач, читается только при включённом
`task_producer`). `clinfer_demo.conf` — минимальная обвязка под totalseg:
запись `models:`, таблица правил, путь к интерпретатору, таймаут.
`clinfer.conf.example.en` документирует все опции сайдкара.

## ABI-скрипты-продюсеры (`abi/`)

Каждый скрипт — обычная программа: clinfer отдаёт ей рабочую папку
(`manifest.json` + `frames/*.raw`), она пишет обратно один `result.json`,
а DICOM-объект пишет уже clinfer. Продюсер никогда не видит DICOM —
контракт в `ABI-README.md`.

- `openvino_detect.py` — детектор OpenVINO; боксы по каждому кадру → **PR**
  (presentation state).
- `sr_llm.py` — вызывает OpenAI-совместимый эндпоинт; возвращает `sr.text` →
  **SR** (структурированный отчёт).
- `totalseg.py` — TotalSegmentator (краниофациальные структуры) → **RTSTRUCT**.
- `stl_mesh.py` — эталонный **STL**-продюсер (мостик seg/stl).
- `xrv.py` — классификатор рентгена грудной клетки; 18 вероятностей
  патологий → **TID 1500** Enhanced SR-измерения.

Также в паке: `threshold_seg.py` (сегментация), `rtstruct_contours.py`
(лицензионно чистые RTSTRUCT-контуры), `clahe.py` / `upscale.py` /
`opacity_patch.py` (фильтры изображений), `xrv_seg.py` / `xrv_regions.py`
(анатомия рентгена), `manifest.example.json` (форма входа),
`requirements.txt`.

> **TID 1500 в Weasis — известный баг апстрима.** Вьюер SR-измерений
> (Enhanced SR SCOORD) имеет два бага отображения, которые мы завели —
> [nroduit/Weasis #922](https://github.com/nroduit/Weasis/issues/922) и
> [#923](https://github.com/nroduit/Weasis/issues/923) — мейнтейнер забрал
> их в апстрим и починил как
> [#931](https://github.com/nroduit/Weasis/issues/931) («Refactor SR
> rendering…», milestone 4.8.0). До выхода 4.8.0 показывайте результаты
> TID 1500 с этой оговоркой. Путь `totalseg.py` → RTSTRUCT-контуры **не
> затронут** (проверено в Weasis 4.7.2, инструмент RT).

## Наблюдение за прогоном

Вся цепочка наблюдаема без отладчика:

- **Лог сервера** (`clarus.log` + stderr): строки `UPS_EVENT` называют каждый
  переход состояния (created → SCHEDULED → IN PROGRESS → COMPLETED);
  `WADO_SERIES` показывает скачивание исследования одной строкой на серию
  (`slices=626 of=626 failed=0`); `STOW_STUDY` фиксирует сохранённый
  результат.
- **Терминал clinfer**: клейм, запуск продюсера и его stderr в реальном
  времени (сайдкар выставляет `PYTHONUNBUFFERED=1`, так что зависшая или
  медленная модель видна сразу).
- **`/metrics`** (Prometheus text format на порту сервера): HTTP-запросы с
  меткой handler, дубликаты/ошибки парсинга STOW, счётчики UPS push
  delivered/retried/dropped.

## clmcp — это демо, а не продукт

clmcp нужен, чтобы показать одно: **роль UPS Task Requester может играть
кто угодно** — агент/MCP-фронтенд, консоль модальности, RIS или дропдаун
вьюера. clmcp говорит только на стандартном DICOMweb (Search → Create →
ожидание по SSE). Замените его своим реквестером — и ничего не изменится,
провод тот же.

![clmcp REPL](../images/screenshot-clmcp.png)

> clmcp REPL — MCP/агентский Task Requester для Clarus. Пять команд в одном
> окне: неизвестная задача честно отказывает со списком известных задач;
> неизвестный пациент отклоняется; уже обработанное исследование отвечает
> «already done» (идемпотентность, без повторного прогона); опечатка
> **Салгалов** резолвится в пациента **Солгалов** честным fuzzymatching'ом
> по PS3.18 (Levenshtein-1, не wildcard) и прогоняет полный цикл UPS-RS —
> TotalSegmentator → RTSTRUCT → «Done»; проверка модальности отказывает
> chestai на CT-исследовании и подсказывает правильную задачу. *(Солгалов =
> фамилия автора, тестовые данные — без PHI.)*

## Результат в Weasis

![totalseg result in Weasis](../images/totalseg_demo.png)

> Weasis 4.7.2 — сохранённый RTSTRUCT отображается контурами в инструменте
> RT: MANDIBLE, TEETH_LOWER, TEETH_UPPER (набор «totalseg-1», Jaw CT
> Segmentation).

## Требования

- **Только несжатые входные изображения.** Демо-бинарники собраны без
  фичи `transcode`; используйте CT в Explicit VR Little Endian. Сжатые
  (JPEG/JPEG2000) входы хранятся как есть, и продюсер получит их всё ещё
  сжатыми.
- Windows x64 или Linux x64.
- Python 3.10+ с TotalSegmentator (`pip install TotalSegmentator`); веса
  ~2 ГБ в `~/.totalsegmentator`, скачайте заранее один раз.
- Weasis 4.7.2+ для RTSTRUCT-контуров; 4.7.3+ для больших STOW-экспортов
  (более ранние сборки обрывают их на 15-секундном `UrlReadTimeout`).
- Продюсеры — **операторски доверенные** программы (та же зона доверия, что
  и ключ API) — см. `ABI-README.md` §1.

## Своя модель

Читайте `ABI-README.md`. Продюсер — любая программа, читающая рабочую папку
и пишущая `result.json`; добавьте запись в `models:` и правило в
`clinfer_demo.conf`. Готово.

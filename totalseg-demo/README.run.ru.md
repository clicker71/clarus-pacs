# totalseg_demo_pack — инструкция по запуску

Самодостаточное демо AI-конвейера поверх DICOMweb: **clarus** (компактный
DICOMweb PACS), **clinfer** (инференс-сайдкар) и **totalseg.py**
(продюсер TotalSegmentator), связанные воедино.

НЕ ДЛЯ ДИАГНОСТИКИ. Это транспорт + исследовательская модель; никаких
клинических утверждений не делается.

## Состав

    clarus(.exe)          однобинарный DICOMweb-сервер (QIDO/WADO/STOW/UPS)
    clinfer(.exe)         инференс-сайдкар (забирает воркитемы, гоняет модели)
    totalseg.py           пример продюсера (craniofacial_structures -> RTSTRUCT)
    clarus_demo.conf      конфиг сервера (порт 8024, ./data)
    clinfer_demo.conf     конфиг сайдкара (правило totalseg, результат rtstruct)
    ABI-README.md         контракт продюсера: пишите свою модель
    ../images/totalseg_demo.png  как выглядит результат в Weasis 4.7.2

## Требования

- Windows x64 или Linux x64.
- Python 3.10+ с установленным TotalSegmentator (`pip install
  TotalSegmentator`). Веса НЕ поставляются: первый запуск скачает ~2 ГБ
  в ~/.totalsegmentator. Скачайте их заранее один раз, вне таймированного
  прогона (см. шапку totalseg.py).
- Любой DICOMweb-клиент для заливки: экспорт из Weasis, curl multipart,
  OHIF или сниппет ниже.
- Вьюер результата: Weasis 4.7.2+ (контуры видны в инструменте RT).

## Сторона Python (разовая настройка)

`totalseg.py` запускается тем `python3`, что лежит в PATH. Настройте
Python-сторону один раз:

1. Создайте venv и поставьте TotalSegmentator. Он подтянет ВСЕ
   зависимости сам (nibabel, nnunetv2, PyTorch, ...) - отдельного
   списка пакетов нет, ничего вручную доставлять и пинить не нужно.
   niftyreg TotalSegmentator не использует.

       python -m venv ts-venv
       ts-venv\Scripts\pip install TotalSegmentator   # Windows
       ts-venv/bin/pip install TotalSegmentator       # Linux/macOS

2. Проверьте установку (nibabel придёт вместе с пакетом):

       ts-venv\Scripts\python -c "import totalsegmentator, nibabel; print('ok')"

3. Скачайте веса заранее один раз, вне таймированного прогона
   (~2 ГБ в ~/.totalsegmentator):

       ts-venv\Scripts\python -m totalsegmentator.bin.totalseg_download_weights

4. Стартуйте clinfer, поставив venv первым в PATH (шаг 2 в «Запуск»).

## Запуск (три терминала)

1. Сервер:

       ./clarus --config clarus_demo.conf          # clarus.exe на Windows

2. Сайдкар (python с TotalSegmentator в PATH):

       ./clinfer --config clinfer_demo.conf        # clinfer.exe на Windows

3. Залейте CT-исследование (STOW-RS), например curl:

       curl -X POST http://127.0.0.1:8024/dicomweb/studies \
            -H "Content-Type: application/dicom" \
            --data-binary @your_slice.dcm

   Сервер сам создаёт воркитем на каждое исследование; clinfer забирает
   CT (челюстная задача) и кладёт RTSTRUCT обратно. Откройте
   исследование в Weasis: контуры появятся в инструменте RT.

## Какие органы? Есть ли настройка точности?

- Демо объявляет три органа: MANDIBLE, TEETH_LOWER, TEETH_UPPER.
  Откройте totalseg.py: сразу под таблицей ORGANS лежит блок
  «HOW TO ADD AN ORGAN» с готовыми записями (SKULL, HEAD,
  SINUS_MAXILLARY, ...). Задача предсказывает семь классов за один
  проход, так что добавление строки ничего не стоит на инференсе.
- Ручки точности НЕТ. Задача обучена на весах 0.5 мм и отказывает
  --fast, то есть каждый прогон уже максимум качества инструмента.
  Единственные тумблеры: какие органы попадают в structure set (ORGANS)
  и где считать модель (TOTALSEG_DEVICE=cpu|cuda).

## Проверка в Weasis (рекомендуется 4.7.3+)

1. Откройте Weasis и добавьте интернет-узел: Описание «Clarus Server»,
   Тип «DICOMweb (all RESTful services)»,
   URL `http://127.0.0.1:8024/dicomweb`, без авторизации.
2. Импортируйте любое CT-изображение с локального устройства в Weasis.
3. Экспортируйте его на узел Clarus (Отправить > узел DICOMweb).
   Берите Weasis 4.7.3+: в 4.7.x раньше длинные STOW-экспорты обрывались
   дефолтным 15-секундным UrlReadTimeout.
4. Подождите несколько минут (зависит от CPU; инференс — раз на
   исследование).
5. Импортируйте с узла DICOMweb: поиск, скачайте того же пациента.
6. Откройте исследование: на CT-серии контуры видны в инструменте RT
   (MANDIBLE / TEETH_LOWER / TEETH_UPPER) - см. ../images/totalseg_demo.png.

## Своя модель

clinfer не привязан к TotalSegmentator. Читайте ABI-README.md: продюсер —
это любая программа, которая читает workdir и пишет result.json.
Добавьте запись в models: и правило в clinfer_demo.conf — готово.

## Примечания

- Только CT: челюстная задача читает единицы Хаунсфилда; продюсер
  отказывает не-CT исследованиям, а clinfer отменяет их воркитемы.
- TotalSegmentator — Apache-2.0 (wasserth/TotalSegmentator). Демо не
  поставляет веса и код модели, кроме обёртки.

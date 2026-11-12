# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Generic DICOM Print Driver (PS3.4 §H) — implements AbstractPrintDriver.

Works with any standard DICOM printer supporting:
  - Basic Film Session SOP Class (1.2.840.10008.5.1.1.1)
  - Basic Film Box SOP Class (1.2.840.10008.5.1.1.2)
  - Basic Grayscale Image Box SOP Class (1.2.840.10008.5.1.1.4)
  - Basic Color Image Box SOP Class (1.2.840.10008.5.1.1.4.1)
  - Printer SOP Class (1.2.840.10008.5.1.1.16)
"""

import io
import logging
import uuid

from pydicom import Dataset, dcmread
from pydicom.uid import generate_uid
from pynetdicom import AE
from pynetdicom.sop_class import (
    BasicColorImageBox,
    BasicColorPrintManagementMeta,
    BasicFilmBox,
    BasicFilmSession,
    BasicGrayscaleImageBox,
    BasicGrayscalePrintManagementMeta,
    Printer,
)

from .base import AbstractPrintDriver, PrintResult

# DIMSE Print status codes (PS3.4 H.4.1)
STATUS_SUCCESS = 0x0000
STATUS_FILM_SESSION_ERROR = 0xB600
STATUS_FILM_BOX_ERROR = 0xB601
STATUS_IMAGE_BOX_ERROR = 0xB602
STATUS_PRINT_FAILURE = 0xB603
STATUS_GENERIC_ERROR = 0xC200

# PS3.7 Annex C: 0xB000-0xBFFF is the WARNING class - the primitive
# COMPLETED and produced a result, so a warning is not a reason to abandon
# the job. 0xB600/B601/B602/B603 are the print-specific WARNING values that
# PS3.4 H.4.1 qualifies as "the film session/film box/image box was created
# but ..."; treating them as failures aborted a job the printer had accepted.
#
# The generic Warning values are PS3.7 Table C.1-1: 0x0107 "Attribute List
# Error" and 0x0116 "Attribute Value Out of Range". Both mean the SCP PROCESSED
# the operation and is telling us about an attribute, which PS3.4 H.4.1 keeps
# out of the Failure class; only 0xAxxx/0xCxxx mean it did not complete.
_WARNING_MIN = 0xB000
_WARNING_MAX = 0xBFFF
_WARNING_CODES = frozenset({0x0107, 0x0116})

# PS3.4 H.4.2.2.4: 0xB603 is "Film Box SOP Instance hierarchy does not
# contain Image Box SOP instances" - the printer has NOTHING to put on the
# film. It is a WARNING in the sense that the primitive completed, but the
# film is blank, so counting it as printed reports a film that does not exist.
STATUS_EMPTY_PAGE = 0xB603


def _is_success_or_warning(status_code):
    """True when a print status means the operation completed.

    PS3.7 Annex C: Success is 0x0000 and the warnings above are non-fatal.
    Only the FAILURE classes (0xAxxx/0xCxxx) mean the operation did not
    complete.
    """
    if status_code == STATUS_SUCCESS:
        return True
    if _WARNING_MIN <= status_code <= _WARNING_MAX:
        return True
    return status_code in _WARNING_CODES


# PS3.4 H.4.1.2: the Film Session N-CREATE attribute set. ImageDisplayFormat,
# FilmOrientation, FilmSizeID and the density/trim attributes are Film BOX
# attributes (H.4.1.2.2) and must not appear in the Film Session request.
#
# SpecificCharacterSet (0008,0005) IS a Film Session attribute (PS3.4
# Table H.4-1): it declares the encoding of the character-set-dependent values
# in the request (FilmSessionLabel, OwnerID). Dropping it made a Cyrillic
# label a missing-attribute failure on a strict SCP, or silently re-encoded.
# MemoryAllocation (2000,0060) is a Film SESSION attribute, not a Film Box one
# - H.4.1.2.2 lists it among the Film Session parameters the SCU supplies.
_FILM_SESSION_ATTRIBUTES = (
    "NumberOfCopies",
    "PrintPriority",
    "MediumType",
    "FilmDestination",
    "FilmSessionLabel",
    "OwnerID",
    "SpecificCharacterSet",
    "MemoryAllocation",
)


class GenericDicomPrintDriver(AbstractPrintDriver):
    """Generic DICOM Print SCU driver (PS3.4 §H).

    Implements the standard N-CREATE Film Session → N-CREATE Film Box →
    N-SET Image Box(es) → N-ACTION Print → N-DELETE Film Session sequence.
    """

    def driver_name(self) -> str:
        return "generic_dicom"

    def print(
        self,
        film_session: Dataset,
        images: list[bytes],
        printer_host: str,
        printer_port: int,
        calling_aet: str,
        called_aet: str | None = None,
    ) -> PrintResult:
        """Execute a DICOM Print job.

        See AbstractPrintDriver.print() for full documentation.

        `calling_aet` is the SCU's OWN AE Title (who we are); `called_aet` is
        the printer's AE Title (who we address). PS3.8 9.3.2.1 makes them two
        separate fields of the A-ASSOCIATE-RQ, and addressing a printer by our
        own title only works when a peer happens to accept anything - a
        configured SCP rejects it as "called AE title not recognised".
        """
        job_id = str(uuid.uuid4())[:8]
        film_boxes_printed = 0
        failed_images = []
        printer_message = None
        submitted_images = 0

        # Determine Image Box SOP Class from first image
        image_box_sop_class = self._image_box_sop_class(images)

        # D8: negotiate the Print Management META SOP Class (PS3.4 H.3/H.4).
        # The meta class is the abstract syntax a conforming printer is
        # registered for; it implies the Film Session, Film Box, Image Box and
        # Printer contexts, and the N-* primitives are then sent with the
        # meta_uid so the SCP can resolve them to the requested instance.
        meta_sop_class = (
            BasicColorPrintManagementMeta
            if image_box_sop_class == BasicColorImageBox
            else BasicGrayscalePrintManagementMeta
        )

        ae = AE(ae_title=calling_aet)
        ae.add_requested_context(meta_sop_class)
        ae.add_requested_context(BasicFilmSession)
        ae.add_requested_context(BasicFilmBox)
        ae.add_requested_context(image_box_sop_class)
        ae.add_requested_context(Printer)

        # 1. Association. D1: the CALLED AE is the printer's, never our own.
        try:
            assoc = ae.associate(
                printer_host,
                printer_port,
                ae_title=(called_aet or calling_aet),
            )
            if not assoc.is_established:
                logging.warning(
                    "[C-PRINT] Association to %s:%d failed",
                    printer_host,
                    printer_port,
                )
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_GENERIC_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"Association to {printer_host}:{printer_port} failed",
                )
        except Exception as e:
            logging.error("[C-PRINT] Association error: %s", e)
            return PrintResult(
                job_id=job_id,
                status=STATUS_GENERIC_ERROR,
                film_boxes_printed=0,
                printer_message=f"Association error: {e}",
            )

        film_session_uid = None
        film_box_uid = None

        try:
            # 2. N-CREATE Film Session
            film_session_uid = generate_uid()
            fs_ds = Dataset()
            # D7: PS3.4 H.4.1 defines the Film Session N-CREATE attribute set
            # as NumberOfCopies, PrintPriority, MediumType, FilmDestination,
            # FilmSessionLabel, OwnerID and (optionally) the privatization
            # attributes. Film Box attributes (ImageDisplayFormat,
            # FilmOrientation, FilmSizeID, ...) belong to the FILM BOX request;
            # copying the whole supplied dataset here sent elements the
            # Film Session IOD does not define, which a strict SCP rejects.
            for attr in _FILM_SESSION_ATTRIBUTES:
                if hasattr(film_session, attr):
                    setattr(fs_ds, attr, getattr(film_session, attr))
            fs_ds.SOPClassUID = BasicFilmSession
            fs_ds.SOPInstanceUID = film_session_uid

            status, fs_response = assoc.send_n_create(
                fs_ds, BasicFilmSession, film_session_uid, meta_uid=meta_sop_class
            )
            status_code = self._status_code(status)
            if not _is_success_or_warning(status_code):
                logging.warning(
                    "[C-PRINT] N-CREATE Film Session failed: status=0x%04X", status_code
                )
                # N-DELETE cleanup not needed — nothing to clean
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_FILM_SESSION_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"N-CREATE Film Session failed: 0x{status_code:04X}",
                )

            logging.debug("[C-PRINT] Film Session created: %s", film_session_uid)

            # 3. N-CREATE Film Box
            film_box_uid = generate_uid()
            fb_ds = Dataset()
            fb_ds.SOPClassUID = BasicFilmBox
            fb_ds.SOPInstanceUID = film_box_uid

            # ReferencedFilmSessionSequence
            ref_fs_item = Dataset()
            ref_fs_item.ReferencedSOPClassUID = BasicFilmSession
            ref_fs_item.ReferencedSOPInstanceUID = film_session_uid
            fb_ds.ReferencedFilmSessionSequence = [ref_fs_item]

            # ImageDisplayFormat — default to "STANDARD\\1,1" if not in film_session
            display_format = getattr(
                film_session, "ImageDisplayFormat", "STANDARD\\1,1"
            )
            fb_ds.ImageDisplayFormat = display_format

            # FilmBoxContentsSequence if present
            if hasattr(film_session, "FilmBoxContentsSequence"):
                fb_ds.FilmBoxContentsSequence = film_session.FilmBoxContentsSequence

            # Copy relevant film session attributes to film box
            for attr in (
                "FilmOrientation",
                "FilmSizeID",
                "MagnificationType",
                "MaxDensity",
                "MinDensity",
                "ConfigurationInformation",
                "AnnotationDisplayFormatID",
                "SmoothingType",
                "BorderDensity",
                "EmptyImageDensity",
                "Trim",
            ):
                if hasattr(film_session, attr):
                    setattr(fb_ds, attr, getattr(film_session, attr))

            status, fb_response = assoc.send_n_create(
                fb_ds, BasicFilmBox, film_box_uid, meta_uid=meta_sop_class
            )
            status_code = self._status_code(status)
            if not _is_success_or_warning(status_code):
                logging.warning(
                    "[C-PRINT] N-CREATE Film Box failed: status=0x%04X", status_code
                )
                # Cleanup: N-DELETE Film Session
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_FILM_BOX_ERROR,
                    film_boxes_printed=0,
                    printer_message=f"N-CREATE Film Box failed: 0x{status_code:04X}",
                )

            logging.debug("[C-PRINT] Film Box created: %s", film_box_uid)

            # D3: the Image Box instances belong to the PRINTER. PS3.4 H.4.1.2.3
            # has the Film Box SCP create them and return their identities in
            # the ReferencedImageBoxSequence of the N-CREATE-RSP; the SCU then
            # addresses THOSE instance UIDs in the N-SET. Generating local
            # UIDs made every N-SET reference an instance the SCP does not
            # have, so a conforming printer rejected the whole film.
            printer_image_box_uids = self._image_box_uids_from_response(fb_response)
            if not printer_image_box_uids:
                logging.error(
                    "[C-PRINT] Film Box N-CREATE returned no "
                    "ReferencedImageBoxSequence - cannot address the Image Boxes"
                )
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_FILM_BOX_ERROR,
                    film_boxes_printed=0,
                    printer_message=(
                        "Film Box N-CREATE returned no ReferencedImageBoxSequence"
                    ),
                )

            # PS3.4 H.4.1.2.3: the ReferencedImageBoxSequence IS the film's
            # capacity - the SCP created exactly that many cells. Images beyond
            # it cannot be placed, so the film would be printed incomplete while
            # the caller was told a film went out. Refuse BEFORE the N-ACTION:
            # once the SCP has printed, the missing cells are unrecoverable.
            if len(images) > len(printer_image_box_uids):
                logging.error(
                    "[C-PRINT] %d image(s) for a %d-cell film - not printing "
                    "an incomplete film",
                    len(images),
                    len(printer_image_box_uids),
                )
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_IMAGE_BOX_ERROR,
                    film_boxes_printed=0,
                    failed_images=[
                        (idx, "No image box position on this film")
                        for idx in range(len(printer_image_box_uids), len(images))
                    ],
                    printer_message=(
                        f"{len(images)} image(s) for a "
                        f"{len(printer_image_box_uids)}-position film - "
                        "film not printed"
                    ),
                )

            # 4. N-SET Image Box × N
            for idx, image_bytes in enumerate(images):
                # D4: ImageBoxPosition (2020,0010) is a REQUIRED attribute of
                # the Basic Image Box N-SET (PS3.4 H.4.1.2.4): it tells the SCP
                # which cell of the film this image occupies. Omitting it is a
                # missing-attribute failure, not a default.
                ib_ds = Dataset()
                ib_ds.SOPClassUID = image_box_sop_class
                ib_ds.ImageBoxPosition = idx + 1

                # Image pixel data in appropriate sequence
                try:
                    img_ds = dcmread(io.BytesIO(image_bytes), force=True)
                except Exception as e:
                    logging.warning("[C-PRINT] Failed to parse image %d: %s", idx, e)
                    failed_images.append((idx, f"Parse error: {e}"))
                    continue

                if image_box_sop_class == BasicColorImageBox:
                    ib_ds.BasicColorImageSequence = [img_ds]
                else:
                    ib_ds.BasicGrayscaleImageSequence = [img_ds]

                # The film defines the cell count and the excess was refused
                # before the print action, so every image here has a cell.
                image_box_uid = printer_image_box_uids[idx]

                status, ib_response = assoc.send_n_set(
                    ib_ds, image_box_sop_class, image_box_uid, meta_uid=meta_sop_class
                )
                status_code = self._status_code(status)
                if not _is_success_or_warning(status_code):
                    logging.warning(
                        "[C-PRINT] N-SET Image Box %d failed: status=0x%04X",
                        idx,
                        status_code,
                    )
                    failed_images.append((idx, f"N-SET error: 0x{status_code:04X}"))
                else:
                    # D2: only an ACCEPTED image counts as submitted.
                    submitted_images += 1
                    if status_code != STATUS_SUCCESS:
                        logging.warning(
                            "[C-PRINT] N-SET Image Box %d warning 0x%04X - "
                            "accepted with the SCP's note",
                            idx,
                            status_code,
                        )
                    logging.debug("[C-PRINT] Image Box %d set: %s", idx, image_box_uid)

            # 5. N-ACTION Print
            #
            # D2: a film with NO accepted image is not printed. N-ACTION on an
            # unpopulated Film Box tells the SCP to produce a blank film (and
            # some SCPs print it), while reporting film_boxes_printed=1 told
            # the caller a film went out when every image had failed.
            if submitted_images == 0:
                logging.error(
                    "[C-PRINT] No image was accepted on the film - not printing"
                )
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_IMAGE_BOX_ERROR,
                    film_boxes_printed=0,
                    failed_images=failed_images,
                    printer_message=(
                        "No image accepted by the printer - film not printed"
                    ),
                )

            na_ds = Dataset()

            # D5: the Print action is sent to the FILM BOX (PS3.4 H.4.1.2.3 -
            # the Basic Film Box SOP Instance is what the Print N-ACTION
            # addresses); the Film Session N-ACTION is an optional
            # session-level operation most SCPs do not implement, and the
            # ReferencedFilmBoxSequence is not an attribute of the request.
            # Sending it to the Film Session asked the SCP to print a session
            # the request did not identify.
            status, na_response = assoc.send_n_action(
                na_ds,
                action_type=1,
                class_uid=BasicFilmBox,
                instance_uid=film_box_uid,
                meta_uid=meta_sop_class,
            )
            status_code = self._status_code(status)
            if not _is_success_or_warning(status_code):
                logging.warning(
                    "[C-PRINT] N-ACTION Print failed: status=0x%04X", status_code
                )
                # Extract printer message from response if available
                if na_response:
                    printer_message = self._extract_status_info(na_response)
                assoc.release()
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_PRINT_FAILURE,
                    film_boxes_printed=0,
                    failed_images=failed_images,
                    printer_message=printer_message
                    or f"N-ACTION Print failed: 0x{status_code:04X}",
                )
            if status_code != STATUS_SUCCESS:
                logging.warning(
                    "[C-PRINT] N-ACTION Print returned warning 0x%04X - the "
                    "film was accepted; check the printer",
                    status_code,
                )
            # PS3.4 H.4.2.2.4: 0xB603 says the Film Box holds no Image Box the
            # printer could put on film. The primitive COMPLETED, so the warning
            # class above lets it through, but no sheet came out - counting it
            # as printed hid a blank print from the caller.
            if status_code == STATUS_EMPTY_PAGE:
                logging.error(
                    "[C-PRINT] Printer reported an EMPTY PAGE (0x%04X) - no "
                    "image reached the film",
                    status_code,
                )
                if na_response:
                    printer_message = self._extract_status_info(na_response)
                return PrintResult(
                    job_id=job_id,
                    status=STATUS_EMPTY_PAGE,
                    film_boxes_printed=0,
                    failed_images=failed_images,
                    printer_message=printer_message
                    or "Empty page: the printer found no Image Box to print",
                )

            # Extract printer status info from N-ACTION response
            if na_response:
                printer_message = self._extract_status_info(na_response)

            logging.info(
                "[C-PRINT] Print job %s: N-ACTION succeeded, %d/%d images",
                job_id,
                submitted_images,
                len(images),
            )

            # Count successfully printed film boxes: the film is printed when
            # the Print action completed, which reaching here means it did.
            film_boxes_printed = 1

            # Determine final status
            if failed_images:
                final_status = STATUS_SUCCESS  # partial success = success with warnings
            else:
                final_status = STATUS_SUCCESS

            return PrintResult(
                job_id=job_id,
                status=final_status,
                film_boxes_printed=film_boxes_printed,
                failed_images=failed_images,
                printer_message=printer_message,
            )

        except Exception as e:
            logging.error("[C-PRINT] Print job %s failed: %s", job_id, e, exc_info=True)
            return PrintResult(
                job_id=job_id,
                status=STATUS_GENERIC_ERROR,
                film_boxes_printed=0,
                failed_images=failed_images,
                printer_message=str(e),
            )

        finally:
            # 6. N-DELETE Film Session (cleanup — always!)
            if film_session_uid:
                self._safe_n_delete(assoc, BasicFilmSession, film_session_uid)

            # 7. Release association
            if assoc.is_established:
                assoc.release()

    # ------------------------------------------------------------------
    # Internal helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _image_box_uids_from_response(fb_response) -> list[str]:
        """The Image Box instance UIDs the PRINTER created for this film.

        PS3.4 H.4.1.2.3: the Film Box N-CREATE-RSP carries a
        ReferencedImageBoxSequence, one item per available film position, and
        each item's Referenced SOP Instance UID is the instance the SCU must
        address in the following N-SET. These are the SCP's instances, not
        ours, so they cannot be generated locally.
        """
        if fb_response is None:
            return []
        uids = []
        try:
            seq = getattr(fb_response, "ReferencedImageBoxSequence", None)
            for item in seq or []:
                uid = str(getattr(item, "ReferencedSOPInstanceUID", "") or "")
                if uid:
                    uids.append(uid)
        except Exception as e:
            logging.debug("[C-PRINT] Could not read ReferencedImageBoxSequence: %s", e)
        return uids

    def _image_box_sop_class(self, images: list[bytes]) -> str:
        """Determine Image Box SOP Class from first image's channels.

        Args:
            images: List of raw DICOM Part 10 byte strings.

        Returns:
            BasicGrayscaleImageBox (1 channel) or BasicColorImageBox (3 channels / RGB).
        """
        if not images:
            return BasicGrayscaleImageBox  # default
        try:
            ds = dcmread(io.BytesIO(images[0]), force=True)
            spp = getattr(ds, "SamplesPerPixel", 1)
            photometric = getattr(ds, "PhotometricInterpretation", "MONOCHROME2")
            if spp == 3 or photometric in ("RGB", "YBR_FULL", "YBR_FULL_422"):
                return BasicColorImageBox
        except Exception as e:
            logging.debug(
                "[C-PRINT] Failed to parse image for SOP class detection: %s", e
            )
        return BasicGrayscaleImageBox

    @staticmethod
    def _status_code(status) -> int:
        """Extract integer status code from pynetdicom Status object.

        Handles both Status objects (with .Status attribute) and raw integers.
        """
        if status is None:
            return STATUS_SUCCESS
        if hasattr(status, "Status"):
            return status.Status
        return int(status)

    @staticmethod
    def _safe_n_delete(assoc, sop_class_uid, sop_instance_uid):
        """Send N-DELETE, logging but not raising on failure."""
        try:
            status, _ = assoc.send_n_delete(sop_class_uid, sop_instance_uid)
            sc = status.Status if hasattr(status, "Status") else status
            if sc != STATUS_SUCCESS:
                logging.debug(
                    "[C-PRINT] N-DELETE returned status=0x%04X for %s",
                    sc,
                    sop_instance_uid,
                )
            else:
                logging.debug("[C-PRINT] Film Session deleted: %s", sop_instance_uid)
        except Exception as e:
            logging.debug("[C-PRINT] N-DELETE failed (non-critical): %s", e)

    @staticmethod
    def _extract_status_info(dataset) -> str | None:
        """Extract human-readable status info from N-ACTION-RSP dataset.

        Looks at PrintManagementStatusSequence or StatusInformation.
        """
        if dataset is None:
            return None
        try:
            # Try StatusInformation (0000,0902)
            si = getattr(dataset, "StatusInformation", None)
            if si:
                return str(si)
            # Try PrintManagementStatusSequence
            pm_seq = getattr(dataset, "PrintManagementStatusSequence", None)
            if pm_seq:
                parts = []
                for item in pm_seq:
                    info = getattr(item, "PrinterStatusInfo", None)
                    if info:
                        parts.append(str(info))
                if parts:
                    return "; ".join(parts)
        except Exception:
            pass
        return None

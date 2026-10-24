"""Tiny translation table: English source strings, Italian translations.

The app is small enough that a dictionary beats a gettext toolchain.
"""
import os

LANGUAGES = ("auto", "en", "it")
LANGUAGE_NAMES = {"auto": "System", "en": "English", "it": "Italiano"}

_IT = {
    # Window
    "Ready": "Pronto",
    "Waiting for the tablet": "In attesa del tablet",
    "Connecting…": "Collegamento…",
    "Connected": "Connesso",
    "Problem": "Problema",
    "Press Connect, then tap “Second screen” in the tablet's quick settings":
        "Premi Collega, poi tocca «Second screen» nel pannello rapido del tablet",
    "Connect": "Collega",
    "Disconnect": "Scollega",
    "Display": "Schermo",
    "Resolution": "Risoluzione",
    "Automatic": "Automatica",
    "Quality": "Qualità",
    "Mbit/s: higher is sharper but uses more bandwidth": "Mbit/s: più alto = più nitido, più banda",
    "Options": "Opzioni",
    "Touch and S Pen": "Touch e S Pen",
    "Sign on Tablet": "Firma sul tablet",
    "In Files, for PDFs, while a tablet is on the network":
        "In File, per i PDF, quando c'è un tablet in rete",
    "On the network now: {name}": "In rete ora: {name}",
    "A tablet is on the network now": "Ora c'è un tablet in rete",
    "No tablet on the network now": "Ora nessun tablet in rete",
    "Ready in the background": "Pronto in background",
    "Starts at login and stays ready after you close this window":
        "Parte all'accesso e resta pronto anche dopo aver chiuso questa finestra",
    "Quick Settings toggle": "Interruttore nelle impostazioni rapide",
    "Adds a “Tablet” button to GNOME's Quick Settings":
        "Aggiunge un pulsante «Tablet» alle impostazioni rapide di GNOME",
    "Open Second Screen from here": "Apri Second Screen da qui",
    "Uses adb: needs wireless debugging on the tablet": "Usa adb: serve il debug wireless sul tablet",
    "adb is not available": "adb non è disponibile",
    "Language": "Lingua",
    "System": "Sistema",
    "Log": "Registro",
    # Notifications and background
    "Tablet connected": "Tablet collegato",
    "Keeps the Quick Settings toggle ready to connect the tablet":
        "Tiene pronto l'interruttore nelle impostazioni rapide per collegare il tablet",
    # Signing
    "Cancel": "Annulla",
    "Done": "Fatto",
    "Undo": "Annulla tratto",
    "Clear page": "Pulisci pagina",
    "My signature": "La mia firma",
    "Keep as my signature": "Salva come mia firma",
    "Pen": "Penna",
    "Eraser": "Gomma",
    "Previous page": "Pagina precedente",
    "Next page": "Pagina successiva",
    "Write with the pen · hold its button to erase": "Scrivi con la penna · tieni premuto il tasto per cancellare",
    "Thickness": "Spessore",
    "Stabilization": "Stabilizzazione",
    "Blue ink": "Inchiostro blu",
    "Black": "Nero",
    "Blue": "Blu",
    "Red": "Rosso",
    "Green": "Verde",
    "Date": "Data",
    "Text": "Testo",
    "Fit": "Adatta",
    "Page {n} of {total}": "Pagina {n} di {total}",
    "Tap where your signature goes": "Tocca dove va la firma",
    "Saved: next time, “My signature” places it with one tap":
        "Salvata: la prossima volta «La mia firma» la mette con un tocco",
    "Tap where the date goes": "Tocca dove va la data",
    "Text to add": "Testo da aggiungere",
    "Place": "Inserisci",
    "Tap where the text goes": "Tocca dove va il testo",
    "Saving…": "Salvataggio…",
    "Could not save: {error}": "Impossibile salvare: {error}",
    "Discard the signature?": "Scartare la firma?",
    "Choose where to save the signed copy": "Scegli dove salvare la copia firmata",
    "Save the signed PDF": "Salva il PDF firmato",
    "Keep signing": "Continua a firmare",
    "Discard": "Scarta",
    "Sign on the tablet": "Firma sul tablet",
    "Tap “Second screen” on the tablet to sign {name}": "Tocca «Second screen» sul tablet per firmare {name}",
    "Sign on this PC": "Firma su questo PC",
    "The tablet did not connect": "Il tablet non si è collegato",
    "You can sign {name} on this PC instead": "Puoi firmare {name} su questo PC",
    "Cannot open {name}": "Impossibile aprire {name}",
    "Signed": "Firmato",
    "Open": "Apri",
    "Show in folder": "Mostra nella cartella",
    "Choose the PDF to sign": "Scegli il PDF da firmare",
    # Engine states
    "Mode {mode} is not supported": "La modalità {mode} non è supportata",
    "Close {apps}: it also uses Wi-Fi Direct": "Chiudi {apps}: usa anche lui il Wi-Fi Direct",
    "Opening Second Screen on the tablet…": "Apro Second Screen sul tablet…",
    "Tap “Second screen” on the tablet": "Tocca «Second screen» sul tablet",
    "The tablet did not answer: open Second Screen on it and connect again":
        "Il tablet non ha risposto: apri Second Screen e collega di nuovo",
    "Connecting to {name}…": "Collegamento a {name}…",
    "The tablet is not answering: close and reopen Second Screen":
        "Il tablet non risponde: chiudi e riapri Second Screen",
}

# Templates that read the same in every language.
NEUTRAL = {"{error}", "{name} · {width}×{height} @ {fps} Hz"}

_current = "en"


def system_language():
    for var in ("LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"):
        value = os.environ.get(var)
        if value:
            return "it" if value.startswith("it") else "en"
    return "en"


def set_language(code):
    global _current
    _current = system_language() if code == "auto" else code


def _(text):
    return _IT.get(text, text) if _current == "it" else text

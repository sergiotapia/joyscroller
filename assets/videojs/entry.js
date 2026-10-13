// The Video.js 10 elements the player uses (see src/player.rs), bundled into
// ../videojs.js by `just videojs`. Each import registers one custom element;
// an element used in the page without its import stays a plain, inert tag.
import '@videojs/html/video/player';
import '@videojs/html/audio/player';
import '@videojs/html/ui/container';
import '@videojs/html/ui/controls';
import '@videojs/html/ui/controls-content';
import '@videojs/html/ui/play-button';
import '@videojs/html/ui/mute-button';
import '@videojs/html/ui/fullscreen-button';
import '@videojs/html/ui/time';
import '@videojs/html/ui/time-group';
import '@videojs/html/ui/time-separator';
import '@videojs/html/ui/time-slider';
import '@videojs/html/ui/volume-slider';
import '@videojs/html/ui/slider-track';
import '@videojs/html/ui/slider-buffer';
import '@videojs/html/ui/slider-fill';
import '@videojs/html/ui/slider-thumb';
import '@videojs/html/ui/slider-preview';
import '@videojs/html/ui/slider-value';
import '@videojs/html/ui/poster';
import '@videojs/html/ui/buffering-indicator';
import '@videojs/html/ui/gesture';
import '@videojs/html/ui/hotkey';
// Only the default icon set, registered up front, rather than every set
// loaded on demand: the bundle is one file.
import '@videojs/html/icons/element/default';
